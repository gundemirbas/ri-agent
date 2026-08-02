//! ACP (Agent Client Protocol) server — headless operation of `ri-agent`.
//!
//! Runs the existing multi-turn agent loop behind the vendor-neutral ACP
//! JSON-RPC surface, so any ACP-capable client (editors, desktop/web UIs,
//! `acpx`, …) can drive ri as a subprocess over stdio.
//!
//! Current coverage (prototype vertical slice):
//! - `initialize` — capability negotiation (protocol v1, image prompts accepted)
//! - `session/new` — in-memory session with its own history + cancel channel
//! - `session/prompt` — streams `agent_message_chunk`, `agent_thought_chunk`,
//!   `tool_call`/`tool_call_update`, and `usage_update` notifications, then
//!   answers with `stopReason: end_turn` (or a JSON-RPC error on failure)
//! - `session/cancel` — maps to ri's `HardAbort` cancel level
//!
//! Known limitations (deliberate, documented): one prompt at a time per
//! session; auto-compaction is disabled (compaction control is a follow-up);
//! `ask_user` is unavailable headless (`request_permission` wiring is a
//! follow-up); multi-turn history is mirrored from streamed events so chained
//! prompts keep their context. The agent loop itself is reused unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, EmbeddedResourceResource,
    InitializeRequest, InitializeResponse, McpCapabilities, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, SessionCapabilities, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent, Result as AcpResult, Stdio, on_receive_notification, on_receive_request,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::agent::types::{AgentEvent, CancelLevel, ToolRegistry};
use crate::agent::{
    AgentLoopConfig, DefaultToolExecutor, FileTracker, ToolOutputLog, run_agent_loop,
};
use crate::app_event::AppEvent;
use crate::context_window::context_window_for_model;
use crate::llm::{AssistantPhase, LlmProvider, UsageStats};
use crate::session_event::SessionEvent;

/// Everything a headless session needs: provider, tools, system prompt.
#[derive(Clone)]
pub struct AcpContext {
    pub provider: Arc<dyn LlmProvider + Send + Sync + 'static>,
    pub model: String,
    pub tools: Arc<ToolRegistry>,
    pub system_prompt: String,
    pub file_tracker: Arc<Mutex<FileTracker>>,
}

/// In-memory per-session state.
struct AcpSession {
    /// Committed history, mirrored from streamed events across prompts.
    events: Vec<SessionEvent>,
    /// Guard against concurrent prompts on the same session.
    running: bool,
    /// Cancel channel for the in-flight prompt, if any.
    cancel_tx: Option<watch::Sender<CancelLevel>>,
}

impl AcpSession {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            running: false,
            cancel_tx: None,
        }
    }
}

type Sessions = Arc<Mutex<HashMap<SessionId, AcpSession>>>;

/// Send an ACP session update notification, ignoring send failures.
///
/// `send_notification` is a synchronous send over the SDK's internal channel;
/// the transport flushes it asynchronously in the background.
macro_rules! send_update {
    ($connection:expr, $session_id:expr, $update:expr) => {{
        let _ =
            $connection.send_notification(SessionNotification::new($session_id.clone(), $update));
    }};
}

/// Run the ACP server on stdio. Blocks until the connection closes.
///
/// Called from a dedicated OS thread via an executor-agnostic block_on; the
/// tokio runtime is reached through `tokio_handle` (tokio channels are
/// executor-agnostic and safe to await from here).
pub async fn run_acp_server(
    ctx: Arc<AcpContext>,
    tokio_handle: tokio::runtime::Handle,
) -> AcpResult<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    let sessions_new = Arc::clone(&sessions);
    let sessions_prompt = Arc::clone(&sessions);
    let sessions_cancel = Arc::clone(&sessions);

    Agent
        .builder()
        .name("ri-agent")
        // ── initialize ─────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version).agent_capabilities(
                        AgentCapabilities::new()
                            .mcp_capabilities(McpCapabilities::new())
                            .session_capabilities(SessionCapabilities::new())
                            .prompt_capabilities(PromptCapabilities::new().image(true)),
                    ),
                )
            },
            on_receive_request!(),
        )
        // ── session/new ────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _connection| {
                let id = SessionId::new(format!("sess-{}", new_session_suffix()));
                sessions_new
                    .lock()
                    .unwrap()
                    .insert(id.clone(), AcpSession::new());
                let _ = req; // cwd is captured; used in a follow-up for FileTracker roots
                responder.respond(NewSessionResponse::new(id))
            },
            on_receive_request!(),
        )
        // ── session/prompt ─────────────────────────────────────────────────
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let session_id = req.session_id.clone();
                let prompt_text = render_prompt_blocks(&req.prompt);

                // 1. Session registry + busy guard; push the user message and
                //    any embedded-resource reads into committed history.
                let (cancel_rx, context_size, history_snapshot) = {
                    let mut map = sessions_prompt.lock().unwrap();
                    let Some(s) = map.get_mut(&session_id) else {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data(format!("unknown session: {session_id}")),
                            )
                            .ok();
                        return Ok(());
                    };
                    if s.running {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("session is busy (one prompt at a time)"),
                            )
                            .ok();
                        return Ok(());
                    }
                    s.running = true;
                    s.events.push(SessionEvent::UserMessage {
                        content: prompt_text.clone(),
                        timestamp: now_ts(),
                    });
                    s.events.extend(synthesize_resource_reads(&req.prompt));
                    let (cancel_tx, cancel_rx) = watch::channel(CancelLevel::None);
                    s.cancel_tx = Some(cancel_tx);
                    (
                        cancel_rx,
                        context_window_for_model(&ctx.model).unwrap_or(200_000),
                        s.events.clone(),
                    )
                };

                // 2. Spawn the existing agent loop on the ri tokio runtime.
                let (tx, rx): (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) =
                    tokio::sync::mpsc::unbounded_channel();
                let mut rx = rx;
                let (_steering_keeper, steering_rx) = tokio::sync::mpsc::unbounded_channel();

                let config = AgentLoopConfig {
                    tools: (*ctx.tools).clone(),
                    file_tracker: Arc::clone(&ctx.file_tracker),
                    tool_output_log: Arc::new(std::sync::Mutex::new(ToolOutputLog::new(
                        &session_id.to_string(),
                    ))),
                    session_events: history_snapshot,
                    current_model: ctx.model.clone(),
                    auto_compaction_enabled: false, // see module docs
                    manual_compaction_instructions: None,
                    executor: Arc::new(DefaultToolExecutor::new()),
                    system_prompt: Some(ctx.system_prompt.clone()),
                };
                let provider = Arc::clone(&ctx.provider);
                let task_tx = tx.clone();
                let join = tokio_handle.spawn(async move {
                    run_agent_loop(config, provider, task_tx, steering_rx, cancel_rx).await;
                });

                // 3. Forward agent events as ACP updates; mirror committed
                //    history so later prompts on this session keep context.
                let mut error: Option<String> = None;
                let mut assistant_text = String::new();
                let mut assistant_thinking: Option<String> = None;
                let mut phase = AssistantPhase::Unknown;
                let mut usage: Option<UsageStats> = None;
                let mut pending_tool: Vec<SessionEvent> = Vec::new();

                while let Some(ev) = rx.recv().await {
                    let AppEvent::Agent(agent_ev) = ev else {
                        continue;
                    };
                    match agent_ev {
                        AgentEvent::Done => break,
                        AgentEvent::Error(e) => {
                            error = Some(e.message.clone());
                            break;
                        }
                        AgentEvent::TextToken { text, phase: ph } => {
                            assistant_text.push_str(&text);
                            if ph != AssistantPhase::Unknown {
                                phase = ph;
                            }
                            if let Some(u) = agent_event_to_update(AgentEvent::TextToken {
                                text,
                                phase: AssistantPhase::Unknown,
                            }) {
                                send_update!(connection, session_id, u);
                            }
                        }
                        AgentEvent::ThinkingToken(t) => {
                            assistant_thinking
                                .get_or_insert_with(String::new)
                                .push_str(&t);
                            send_update!(
                                connection,
                                session_id,
                                SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(t.clone())),
                                ))
                            );
                        }
                        AgentEvent::Usage(u) => {
                            usage = Some(u);
                            if let Some(uu) = usage_update_value(&u, context_size as u64) {
                                send_update!(
                                    connection,
                                    session_id,
                                    SessionUpdate::UsageUpdate(uu)
                                );
                            }
                        }
                        AgentEvent::ToolCallStart { id, name, args } => {
                            pending_tool.push(SessionEvent::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                args,
                                include_in_llm: true,
                                timestamp: now_ts(),
                            });
                            let kind = tool_kind(&name);
                            send_update!(
                                connection,
                                session_id,
                                SessionUpdate::ToolCall(
                                    ToolCall::new(id, name.clone())
                                        .kind(kind)
                                        .status(ToolCallStatus::Pending)
                                )
                            );
                        }
                        AgentEvent::ToolCallEnd { id, result } => {
                            pending_tool.push(SessionEvent::ToolResult {
                                id: id.clone(),
                                name: String::new(),
                                content: result.content.as_text().to_string(),
                                is_error: result.is_error,
                                display_range: None,
                                include_in_llm: true,
                                timestamp: now_ts(),
                            });
                            let content = result.content.as_text().to_string();
                            send_update!(
                                connection,
                                session_id,
                                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                    id,
                                    ToolCallUpdateFields::new()
                                        .status(ToolCallStatus::Completed)
                                        .content(vec![ToolCallContent::from(ContentBlock::Text(
                                            TextContent::new(content)
                                        ),)]),
                                ))
                            );
                        }
                        AgentEvent::TurnEnd => {
                            commit_turn(
                                &sessions_prompt,
                                &session_id,
                                &mut assistant_text,
                                &mut assistant_thinking,
                                phase,
                                usage,
                                &pending_tool,
                            );
                            pending_tool.clear();
                            assistant_text.clear();
                            assistant_thinking = None;
                            phase = AssistantPhase::Unknown;
                            usage = None;
                        }
                        // Not surfaced headless (display-only or future work):
                        AgentEvent::ToolCallIntent { .. }
                        | AgentEvent::ToolCallArgsDelta { .. }
                        | AgentEvent::ToolOutputChunk { .. }
                        | AgentEvent::SteeringConsumed { .. }
                        | AgentEvent::StatusUpdate(_)
                        | AgentEvent::Compacting
                        | AgentEvent::CompactionDone(_)
                        | AgentEvent::ExternalFileChange { .. } => {}
                    }
                }

                // 4. Release the session; wait for (already finished) loop.
                {
                    let mut map = sessions_prompt.lock().unwrap();
                    if let Some(s) = map.get_mut(&session_id) {
                        s.running = false;
                        s.cancel_tx = None;
                    }
                }
                let _ = join.await;

                // 5. Reply to the original prompt request.
                match error {
                    Some(msg) => responder.respond_with_error(
                        agent_client_protocol::Error::internal_error().data(msg),
                    ),
                    None => responder.respond(PromptResponse::new(StopReason::EndTurn)),
                }
            },
            on_receive_request!(),
        )
        // ── session/cancel ──────────────────────────────────────────────────
        .on_receive_notification(
            async move |notif: CancelNotification, _connection| {
                let mut map = sessions_cancel.lock().unwrap();
                if let Some(s) = map.get_mut(&notif.session_id)
                    && let Some(tx) = s.cancel_tx.as_ref()
                {
                    let _ = tx.send(CancelLevel::HardAbort);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

// ── Forwarding / mirroring helpers ────────────────────────────────────────────

/// Commit one completed assistant turn (message + tool pairs) into the
/// session's mirrored history.
#[allow(clippy::too_many_arguments)]
fn commit_turn(
    sessions: &Sessions,
    session_id: &SessionId,
    assistant_text: &mut String,
    assistant_thinking: &mut Option<String>,
    phase: AssistantPhase,
    usage: Option<UsageStats>,
    pending_tool: &[SessionEvent],
) {
    let mut map = sessions.lock().unwrap();
    let Some(s) = map.get_mut(session_id) else {
        return;
    };
    let has_tools = pending_tool
        .iter()
        .any(|e| matches!(e, SessionEvent::ToolCall { .. }));
    if !assistant_text.is_empty() || has_tools {
        s.events.push(SessionEvent::AssistantMessage {
            content: std::mem::take(assistant_text),
            thinking: assistant_thinking.take(),
            phase: if phase == AssistantPhase::Unknown {
                AssistantPhase::Final
            } else {
                phase
            },
            usage,
            timestamp: now_ts(),
        });
    }
    s.events.extend(pending_tool.iter().cloned());
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

/// Flatten ACP prompt content blocks into a user-message string.
fn render_prompt_blocks(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(img) => {
                let uri = format!("data:{};base64,{}", img.mime_type, img.data);
                parts.push(format!("![image]({uri})\n[Image attached by client]"));
            }
            ContentBlock::ResourceLink(r) => {
                parts.push(format!("(see attached: {})", r.uri));
            }
            ContentBlock::Resource(r) => {
                parts.push(format!("(see attached: {})", resource_uri(&r.resource)));
            }
            _ => parts.push("(unsupported content block)".to_string()),
        }
    }
    parts.join("\n\n")
}

fn resource_uri(r: &EmbeddedResourceResource) -> String {
    match r {
        EmbeddedResourceResource::TextResourceContents(t) => t.uri.clone(),
        EmbeddedResourceResource::BlobResourceContents(b) => b.uri.clone(),
        _ => String::new(),
    }
}

fn resource_text(r: &EmbeddedResourceResource) -> Option<String> {
    match r {
        EmbeddedResourceResource::TextResourceContents(t) => Some(t.text.clone()),
        EmbeddedResourceResource::BlobResourceContents(_) => None,
        _ => None,
    }
}

/// For embedded `Resource` blocks, synthesize the same `read_file` tool-call +
/// result pair the `@file` attachment path uses, so file contents reach the LLM
/// without a round trip.
fn synthesize_resource_reads(blocks: &[ContentBlock]) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        let ContentBlock::Resource(r) = block else {
            continue;
        };
        let uri = resource_uri(&r.resource);
        let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
        let Some(content) = resource_text(&r.resource) else {
            continue;
        };
        let ts = now_ts();
        let id = format!("attach_{idx}");
        out.push(SessionEvent::ToolCall {
            id: id.clone(),
            name: "read_file".to_string(),
            args: serde_json::json!({ "path": path }),
            include_in_llm: true,
            timestamp: ts,
        });
        out.push(SessionEvent::ToolResult {
            id,
            name: "read_file".to_string(),
            content,
            is_error: false,
            display_range: None,
            include_in_llm: true,
            timestamp: ts,
        });
    }
    out
}

// ── AgentEvent → ACP SessionUpdate (display-only events) ──────────────────────

/// Map a visible `AgentEvent` to an ACP `SessionUpdate` (or `None` when the
/// event carries nothing a client should render — those are mirrored by the
/// main loop where needed).
fn agent_event_to_update(ev: AgentEvent) -> Option<SessionUpdate> {
    match ev {
        AgentEvent::TextToken { text, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text))),
        )),
        _ => None,
    }
}

fn usage_update_value(usage: &UsageStats, context_size: u64) -> Option<UsageUpdate> {
    let used = usage.used_tokens()? as u64;
    Some(UsageUpdate::new(used.min(context_size), context_size))
}

/// Map a ri tool name to an ACP `ToolKind`.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "read_skill" => ToolKind::Read,
        "edit_file" | "write_file" => ToolKind::Edit,
        "find_files" => ToolKind::Search,
        "bash" | "exec" => ToolKind::Execute,
        "invoke_subagent" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

// ── small helpers ─────────────────────────────────────────────────────────────

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn new_session_suffix() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    format!(
        "{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    )
}

/// Build context for an ACP server: provider, built-in tools, system prompt.
pub async fn build_context(
    provider: Arc<dyn LlmProvider + Send + Sync + 'static>,
    model: &str,
    cwd: &std::path::Path,
) -> AcpContext {
    let file_tracker = Arc::new(Mutex::new(FileTracker::with_exclusions(vec![], &[])));
    let skills = Arc::new(crate::skills::load_skills());
    let tools = crate::agent::tools::register_builtin_tools(
        None,
        Arc::clone(&file_tracker),
        Arc::clone(&skills),
        Vec::new(),
    )
    .await;
    let system_prompt =
        crate::agent::build_system_prompt(&tools, &cwd.to_string_lossy(), &skills, None);

    AcpContext {
        provider,
        model: model.to_string(),
        tools: Arc::new(tools),
        system_prompt,
        file_tracker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::AssistantPhase;

    #[test]
    fn maps_text_token_to_agent_message_chunk() {
        let update = agent_event_to_update(AgentEvent::TextToken {
            text: "hello".to_string(),
            phase: AssistantPhase::Unknown,
        })
        .unwrap();
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let ContentBlock::Text(t) = chunk.content else {
                    panic!("expected text block");
                };
                assert_eq!(t.text, "hello");
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn maps_tool_kind_names() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("find_files"), ToolKind::Search);
        assert_eq!(tool_kind("mystery_tool"), ToolKind::Other);
    }

    #[test]
    fn suppresses_display_only_events() {
        assert!(
            agent_event_to_update(AgentEvent::ToolOutputChunk {
                id: "x".into(),
                chunk: "y".into(),
            })
            .is_none()
        );
        assert!(agent_event_to_update(AgentEvent::Done).is_none());
    }

    #[test]
    fn renders_prompt_blocks_with_text_and_resource() {
        use agent_client_protocol::schema::v1::{
            EmbeddedResource, EmbeddedResourceResource, TextResourceContents,
        };

        let blocks = vec![
            ContentBlock::Text(TextContent::new("analyze this")),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "print(1)",
                    "file:///abs/main.py",
                )),
            )),
        ];
        let text = render_prompt_blocks(&blocks);
        assert!(text.contains("analyze this"));
        assert!(text.contains("/abs/main.py"));

        let events = synthesize_resource_reads(&blocks);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            SessionEvent::ToolCall { name, .. } if name == "read_file"
        ));
    }

    #[tokio::test]
    async fn usage_update_value_uses_context_window() {
        let usage = UsageStats {
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        let u = usage_update_value(&usage, 200_000).expect("usage");
        assert_eq!(u.used, 150);
        assert_eq!(u.size, 200_000);
    }
}
