//! ACP (Agent Client Protocol) server — headless operation of `ri-agent`.
//!
//! Runs the existing multi-turn agent loop behind the vendor-neutral ACP
//! JSON-RPC surface, so any ACP-capable client (editors, desktop/web UIs,
//! `acpx`, …) can drive ri as a subprocess (stdio) or over WebSocket.
//!
//! Implemented surface:
//! - `initialize` — protocol v1, capability negotiation (image prompts,
//!   in-memory `session/load`)
//! - `session/new` — in-memory session (history, cancel channel, cwd)
//! - `session/load` — replays a known session's history as updates; sessions
//!   are persisted to disk (`~/.local/share/ri/sessions/acp/<id>.json`) after
//!   each prompt, so they can be resumed by a later process
//! - `session/prompt` — streams `agent_message_chunk`, `agent_thought_chunk`,
//!   `tool_call`/`tool_call_update` (with live tool output forwarded as
//!   in-progress `tool_call_update` chunks), `usage_update`, then `end_turn`
//! - `session/cancel` — maps to ri `HardAbort`
//! - `ask_user` → `session/request_permission` (multiple-choice mapping;
//!   freeform-only asks surface a single "Continue" option)
//! - Custom `_ri/*` methods: `_ri/get_state`, `_ri/set_model`,
//!   `_ri/set_thinking` (provider is rebuilt on change),
//!   `_ri/list_sessions` (persisted sessions, newest first)
//!
//! Limitations (deliberate, documented): one prompt at a time per session;
//! tool cwd is the current process directory (per-session cwd feeds the
//! system prompt; follow-up makes it a real per-session root). The agent loop
//! itself is reused unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::{fs, path::Path};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionCapabilities, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent, Client, ConnectTo, JsonRpcRequest, JsonRpcResponse, Result as AcpResult, Stdio,
    on_receive_notification, on_receive_request,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::agent::tools::register_builtin_tools;
use crate::agent::types::{AgentEvent, AskRequest, AskUserResponse, CancelLevel};
use crate::agent::{
    AgentLoopConfig, DefaultToolExecutor, FileTracker, ToolOutputLog, run_agent_loop,
};
use crate::app_event::AppEvent;
use crate::context_window::context_window_for_model;
use crate::llm::{AssistantPhase, LlmProvider, UsageStats};
use crate::session_event::SessionEvent;
use crate::thinking::ThinkingLevel;

// ── Ri-specific custom RPC (item: provider/thinking surface) ─────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/get_state", response = RiGetStateResponse)]
struct RiGetStateRequest;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiGetStateResponse {
    model: String,
    thinking: String,
    sessions: Vec<String>,
    streaming_sessions: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/set_model", response = RiSetModelResponse)]
struct RiSetModelRequest {
    #[serde(default)]
    model: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiSetModelResponse {
    ok: bool,
    error: Option<String>,
    model: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/set_thinking", response = RiSetThinkingResponse)]
struct RiSetThinkingRequest {
    #[serde(default)]
    level: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiSetThinkingResponse {
    ok: bool,
    error: Option<String>,
    level: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/list_sessions", response = RiListSessionsResponse)]
struct RiListSessionsRequest;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiListSessionsResponse {
    sessions: Vec<RiSessionMeta>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RiSessionMeta {
    id: String,
    cwd: String,
    updated: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/delete_session", response = RiDeleteSessionResponse)]
struct RiDeleteSessionRequest {
    #[serde(default, rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiDeleteSessionResponse {
    ok: bool,
    error: Option<String>,
    deleted_in_memory: bool,
    deleted_on_disk: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/prune_sessions", response = RiPruneSessionsResponse)]
struct RiPruneSessionsRequest {
    #[serde(default, rename = "olderThanSeconds")]
    older_than_seconds: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiPruneSessionsResponse {
    ok: bool,
    deleted: usize,
}

// ── Context ───────────────────────────────────────────────────────────────────

/// Rebuilds the active provider for a given model + thinking level (used by
/// the `_ri/set_model` / `_ri/set_thinking` custom methods).
pub type ProviderRebuild = Arc<
    dyn Fn(&str, ThinkingLevel) -> anyhow::Result<Arc<dyn LlmProvider + Send + Sync>> + Send + Sync,
>;

/// Everything a headless server needs: the current provider (swappable), the
/// current model/thinking level, a provider rebuild hook, and shared tool
/// prerequisites.
pub struct AcpContext {
    pub provider: RwLock<Arc<dyn LlmProvider + Send + Sync + 'static>>,
    pub model: RwLock<String>,
    pub thinking: RwLock<ThinkingLevel>,
    pub rebuild: ProviderRebuild,
    /// Optional explicit tokio runtime handle. Set in stdio mode (handlers run
    /// on the ACP async-io executor, so they cannot call
    /// [`tokio::runtime::Handle::current`]); `None` in WebSocket mode where
    /// handlers run inside tokio/axum and use [`tokio::runtime::Handle::current`].
    pub tokio_handle: Option<tokio::runtime::Handle>,
    pub file_tracker: Arc<Mutex<FileTracker>>,
    pub skills: Arc<Vec<crate::skills::SkillMeta>>,
}

/// In-memory per-session state.
struct AcpSession {
    events: Vec<SessionEvent>,
    running: bool,
    cancel_tx: Option<watch::Sender<CancelLevel>>,
    #[allow(dead_code)] // kept for future tool-cwd wiring; session prompt uses it
    cwd: std::path::PathBuf,
}

impl AcpSession {
    fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            events: Vec::new(),
            running: false,
            cancel_tx: None,
            cwd,
        }
    }
}

type Sessions = Arc<Mutex<HashMap<SessionId, AcpSession>>>;

/// Send an ACP session update notification, ignoring send failures.
macro_rules! send_update {
    ($connection:expr, $session_id:expr, $update:expr) => {{
        let _ =
            $connection.send_notification(SessionNotification::new($session_id.clone(), $update));
    }};
}

/// Build the agent component (handlers registered) shared by the stdio and
/// WebSocket transports. Returns a builder that implements `ConnectTo<Client>`.
fn build_agent(
    ctx: Arc<AcpContext>,
    sessions: Sessions,
) -> impl agent_client_protocol::ConnectTo<Client> {
    let s_new = Arc::clone(&sessions);
    let s_load = Arc::clone(&sessions);
    let s_prompt = Arc::clone(&sessions);
    let s_cancel = Arc::clone(&sessions);
    let s_state = Arc::clone(&sessions);
    let s_del = Arc::clone(&sessions);
    let ctx_state = Arc::clone(&ctx);
    let ctx_set_model = Arc::clone(&ctx);
    let ctx_set_thinking = Arc::clone(&ctx);

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
                            .prompt_capabilities(PromptCapabilities::new().image(true))
                            .load_session(true),
                    ),
                )
            },
            on_receive_request!(),
        )
        // ── session/new ────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _connection| {
                let id = SessionId::new(format!("sess-{}", new_session_suffix()));
                s_new
                    .lock()
                    .unwrap()
                    .insert(id.clone(), AcpSession::new(req.cwd.clone()));
                responder.respond(NewSessionResponse::new(id))
            },
            on_receive_request!(),
        )
        // ── session/load (in-memory or disk resume) ───────────────────────
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, connection| {
                // Prefer a live in-memory session; fall back to a persisted
                // session from a previous run (disk resume).
                let in_memory = {
                    let map = s_load.lock().unwrap();
                    map.get(&req.session_id)
                        .map(|s| (s.running, s.events.clone(), s.cwd.clone()))
                };
                let (events, _cwd) = match in_memory {
                    Some((true, _, _)) => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("session is busy; cannot load while a prompt runs"),
                            )
                            .ok();
                        return Ok(());
                    }
                    Some((false, events, cwd)) => (events, cwd),
                    None => {
                        let Some(persisted) = read_persisted_session(&req.session_id) else {
                            responder
                                .respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data("unknown session"),
                                )
                                .ok();
                            return Ok(());
                        };
                        // Register in memory so subsequent prompts resume from
                        // the restored history.
                        let mut map = s_load.lock().unwrap();
                        let slot = map
                            .entry(req.session_id.clone())
                            .or_insert_with(|| AcpSession::new(persisted.cwd.clone()));
                        slot.events = persisted.events.clone();
                        (persisted.events.clone(), persisted.cwd)
                    }
                };
                for update in session_replay_updates(&events) {
                    send_update!(connection, req.session_id, update);
                }
                responder.respond(LoadSessionResponse::new())
            },
            on_receive_request!(),
        )
        // ── session/prompt ─────────────────────────────────────────────────
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let session_id = req.session_id.clone();
                let prompt_text = render_prompt_blocks(&req.prompt);

                // Per-session (cwd, busy guard) + push user message / resource reads.
                let (session_cwd, cancel_rx, context_size, history_snapshot) = {
                    let mut map = s_prompt.lock().unwrap();
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
                    let cwd = s.cwd.clone();
                    (
                        cwd,
                        cancel_rx,
                        context_window_for_model(&ctx.model.read().unwrap()).unwrap_or(200_000),
                        s.events.clone(),
                    )
                };

                // Channel for agent events AND ask_user replies.
                let (tx, rx): (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) =
                    tokio::sync::mpsc::unbounded_channel();
                let mut rx = rx;

                // Tools are registered per prompt so `ask_user` can route its
                // question back through this prompt's channel (headless mode).
                let tools = register_builtin_tools(
                    Some(tx.clone()),
                    Arc::clone(&ctx.file_tracker),
                    Arc::clone(&ctx.skills),
                    Vec::new(),
                )
                .await;
                let system_prompt = crate::agent::build_system_prompt(
                    &tools,
                    &session_cwd.to_string_lossy(),
                    &ctx.skills,
                    None,
                );
                let provider = ctx.provider.read().unwrap().clone();
                let model = ctx.model.read().unwrap().clone();
                let tokio_handle = ctx
                    .tokio_handle
                    .clone()
                    .unwrap_or_else(tokio::runtime::Handle::current);

                let config = AgentLoopConfig {
                    tools,
                    file_tracker: Arc::clone(&ctx.file_tracker),
                    tool_output_log: Arc::new(std::sync::Mutex::new(ToolOutputLog::new(
                        session_id.to_string().as_str(),
                    ))),
                    session_events: history_snapshot,
                    current_model: model,
                    auto_compaction_enabled: true,
                    manual_compaction_instructions: None,
                    executor: Arc::new(DefaultToolExecutor::with_root(session_cwd.clone())),
                    system_prompt: Some(system_prompt),
                };
                let (steering_keeper, steering_rx) = tokio::sync::mpsc::unbounded_channel();
                let _ = steering_keeper;
                let provider_loop = Arc::clone(&provider);
                let task_tx = tx.clone();
                let join = tokio_handle.spawn(async move {
                    run_agent_loop(config, provider_loop, task_tx, steering_rx, cancel_rx).await;
                });

                // Forward agent events + answer ask_user via request_permission.
                let mut error: Option<String> = None;
                let mut assistant_text = String::new();
                let mut assistant_thinking: Option<String> = None;
                let mut phase = AssistantPhase::Unknown;
                let mut usage: Option<UsageStats> = None;
                let mut pending_tool: Vec<SessionEvent> = Vec::new();

                while let Some(ev) = rx.recv().await {
                    match ev {
                        AppEvent::AskUser(request) => {
                            let (options, option_titles) = permission_options(&request);
                            let mut title = request.question.clone();
                            if let Some(ctx_text) = request.context.as_deref() {
                                title = format!("{title}\n\n{ctx_text}");
                            }
                            let tool_call = ToolCallUpdate::new(
                                format!("ask-{}", now_ts()),
                                ToolCallUpdateFields::new()
                                    .title(title.clone())
                                    .kind(ToolKind::Other)
                                    .status(ToolCallStatus::InProgress),
                            );
                            let outcome = connection
                                .send_request(RequestPermissionRequest::new(
                                    session_id.clone(),
                                    tool_call,
                                    options,
                                ))
                                .block_task()
                                .await;
                            let answer = ask_reply_from_outcome(&outcome, &option_titles);
                            let _ = request.reply.send(answer);
                        }
                        AppEvent::Agent(agent_ev) => match agent_ev {
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
                                send_update!(
                                    connection,
                                    session_id,
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(text)),
                                    ))
                                );
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
                                            .content(vec![ToolCallContent::from(
                                                ContentBlock::Text(TextContent::new(content)),
                                            )]),
                                    ))
                                );
                            }
                            AgentEvent::TurnEnd => {
                                commit_turn(
                                    &s_prompt,
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
                            AgentEvent::ToolOutputChunk { id, chunk } => {
                                // Live tool output: stream each chunk as an
                                // in-progress `tool_call_update` so headless
                                // clients render bash/exec output as it runs
                                // (the final `Completed` update arrives on
                                // `ToolCallEnd`).
                                send_update!(
                                    connection,
                                    session_id,
                                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                        id,
                                        ToolCallUpdateFields::new()
                                            .status(ToolCallStatus::InProgress)
                                            .content(vec![ToolCallContent::from(
                                                ContentBlock::Text(TextContent::new(chunk)),
                                            )]),
                                    ))
                                );
                            }
                            AgentEvent::ToolCallIntent { .. }
                            | AgentEvent::ToolCallArgsDelta { .. }
                            | AgentEvent::SteeringConsumed { .. }
                            | AgentEvent::StatusUpdate(_)
                            | AgentEvent::Compacting
                            | AgentEvent::CompactionDone(_)
                            | AgentEvent::ExternalFileChange { .. } => {}
                        },
                        _ => {}
                    }
                }

                {
                    let mut map = s_prompt.lock().unwrap();
                    if let Some(s) = map.get_mut(&session_id) {
                        s.running = false;
                        s.cancel_tx = None;
                    }
                }
                let _ = join.await;

                // Persist the committed history + cwd so the session can be
                // resumed from disk by a later process (session/load).
                {
                    let map = s_prompt.lock().unwrap();
                    if let Some(s) = map.get(&session_id)
                        && !s.events.is_empty()
                    {
                        persist_session(&session_id, &s.events, &s.cwd);
                    }
                }

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
                let mut map = s_cancel.lock().unwrap();
                if let Some(s) = map.get_mut(&notif.session_id)
                    && let Some(tx) = s.cancel_tx.as_ref()
                {
                    let _ = tx.send(CancelLevel::HardAbort);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        // ── _ri/get_state ───────────────────────────────────────────────────
        .on_receive_request(
            async move |_req: RiGetStateRequest, responder, _connection| {
                let model = ctx_state.model.read().unwrap().clone();
                let thinking = ctx_state.thinking.read().unwrap().as_str().to_string();
                {
                    let map = s_state.lock().unwrap();
                    let sessions = map.keys().map(|s| s.to_string()).collect::<Vec<_>>();
                    let streaming = map.values().filter(|s| s.running).count();
                    responder.respond(RiGetStateResponse {
                        model,
                        thinking,
                        sessions,
                        streaming_sessions: streaming,
                    })
                }
            },
            on_receive_request!(),
        )
        // ── _ri/list_sessions (persisted, newest first) ──────────────────────
        .on_receive_request(
            async move |_req: RiListSessionsRequest, responder, _connection| {
                let sessions = persisted_session_list()
                    .into_iter()
                    .map(|p| RiSessionMeta {
                        id: p.id,
                        cwd: p.cwd.to_string_lossy().into_owned(),
                        updated: p.updated,
                    })
                    .collect();
                responder.respond(RiListSessionsResponse { sessions })
            },
            on_receive_request!(),
        )
        // ── _ri/delete_session ───────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiDeleteSessionRequest, responder, _connection| {
                if req.session_id.trim().is_empty() {
                    return responder.respond(RiDeleteSessionResponse {
                        ok: false,
                        error: Some("sessionId must not be empty".to_string()),
                        deleted_in_memory: false,
                        deleted_on_disk: false,
                    });
                }
                let id = SessionId::new(req.session_id.clone());

                // Remember whether a disk file existed so we can report it.
                let mut deleted_in_memory = false;
                let mut error: Option<String> = None;
                {
                    let mut map = s_del.lock().unwrap();
                    if let Some(s) = map.get(&id) {
                        if s.running {
                            error =
                                Some("session is busy; cancel the active prompt first".to_string());
                        } else {
                            map.remove(&id);
                            deleted_in_memory = true;
                        }
                    }
                }
                if error.is_some() {
                    return responder.respond(RiDeleteSessionResponse {
                        ok: false,
                        error,
                        deleted_in_memory,
                        deleted_on_disk: false,
                    });
                }

                let deleted_on_disk = delete_persisted_session(&id);
                responder.respond(RiDeleteSessionResponse {
                    ok: true,
                    error: None,
                    deleted_in_memory,
                    deleted_on_disk,
                })
            },
            on_receive_request!(),
        )
        // ── _ri/prune_sessions (retention) ──────────────────────────────────
        .on_receive_request(
            async move |req: RiPruneSessionsRequest, responder, _connection| {
                let cutoff = now_ts().saturating_sub(req.older_than_seconds);
                let mut deleted = 0usize;
                if let Some(dir) = acp_sessions_dir() {
                    let Ok(entries) = fs::read_dir(&dir) else {
                        return responder.respond(RiPruneSessionsResponse { ok: true, deleted });
                    };
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|x| x.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(data) = fs::read_to_string(&path)
                            && let Ok(p) = serde_json::from_str::<PersistedSession>(&data)
                            && p.updated <= cutoff
                        {
                            let _ = fs::remove_file(&path);
                            deleted += 1;
                        }
                    }
                }
                responder.respond(RiPruneSessionsResponse { ok: true, deleted })
            },
            on_receive_request!(),
        )
        // ── _ri/set_model ───────────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiSetModelRequest, responder, _connection| {
                if req.model.trim().is_empty() {
                    return responder.respond(RiSetModelResponse {
                        ok: false,
                        error: Some("model must not be empty".to_string()),
                        model: req.model,
                    });
                }
                let thinking = *ctx_set_model.thinking.read().unwrap();
                match (ctx_set_model.rebuild)(&req.model, thinking) {
                    Ok(provider) => {
                        *ctx_set_model.model.write().unwrap() = req.model.clone();
                        *ctx_set_model.provider.write().unwrap() = provider;
                        responder.respond(RiSetModelResponse {
                            ok: true,
                            error: None,
                            model: req.model,
                        })
                    }
                    Err(e) => responder.respond(RiSetModelResponse {
                        ok: false,
                        error: Some(e.to_string()),
                        model: req.model,
                    }),
                }
            },
            on_receive_request!(),
        )
        // ── _ri/set_thinking ───────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiSetThinkingRequest, responder, _connection| {
                let Some(level) = ThinkingLevel::parse(&req.level) else {
                    return responder.respond(RiSetThinkingResponse {
                        ok: false,
                        error: Some(format!("unknown thinking level '{}'", req.level)),
                        level: req.level,
                    });
                };
                let model = ctx_set_thinking.model.read().unwrap().clone();
                match (ctx_set_thinking.rebuild)(&model, level) {
                    Ok(provider) => {
                        *ctx_set_thinking.thinking.write().unwrap() = level;
                        *ctx_set_thinking.provider.write().unwrap() = provider;
                        responder.respond(RiSetThinkingResponse {
                            ok: true,
                            error: None,
                            level: level.as_str().to_string(),
                        })
                    }
                    Err(e) => responder.respond(RiSetThinkingResponse {
                        ok: false,
                        error: Some(e.to_string()),
                        level: req.level,
                    }),
                }
            },
            on_receive_request!(),
        )
}

// ── Transport entry points ────────────────────────────────────────────────────

/// Run the ACP server on stdio. Blocks until the connection closes.
///
/// Called from a dedicated OS thread via an executor-agnostic block_on; ri's
/// tokio runtime is reached through a captured handle (tokio channels are
/// executor-agnostic).
pub async fn run_acp_server(
    ctx: Arc<AcpContext>,
    _tokio_handle: tokio::runtime::Handle,
) -> AcpResult<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    build_agent(ctx, sessions).connect_to(Stdio::new()).await
}

/// Run the ACP over HTTP + WebSocket on `addr` (axum).
pub async fn run_acp_ws(ctx: Arc<AcpContext>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let factory = {
        let ctx = Arc::clone(&ctx);
        let sessions = sessions.clone();
        move || build_agent(ctx.clone(), sessions.clone())
    };
    let server = agent_client_protocol_http::AcpHttpServer::new(factory);
    let router = server.into_router();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

// ── Mirroring helpers ─────────────────────────────────────────────────────────

/// Turn a session's committed history into ACP update notifications that a
/// client can replay on `session/load`.
fn session_replay_updates(events: &[SessionEvent]) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for ev in events {
        match ev {
            SessionEvent::UserMessage { content, .. } => {
                updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(content.clone())),
                )))
            }
            SessionEvent::AssistantMessage { content, .. } => {
                updates.push(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(content.clone())),
                )))
            }
            _ => {}
        }
    }
    updates
}

/// Build ACP permission options for an `ask_user` request. Each `ask_user`
/// option maps to an `AllowOnce` choice (option `description` is folded into
/// the visible label). When the ask also allows freeform input — or has no
/// listed options at all — a trailing "Continue" escape is added so the
/// headless client can always proceed without picking a listed option.
fn permission_options(request: &AskRequest) -> (Vec<PermissionOption>, Vec<(String, String)>) {
    let mut options: Vec<PermissionOption> = request
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let label = match o.description.as_deref() {
                Some(d) if !d.is_empty() => format!("{} — {d}", o.title),
                _ => o.title.clone(),
            };
            permission_option(format!("opt-{i}"), &label)
        })
        .collect();
    if request.allow_freeform || options.is_empty() {
        options.push(permission_option("continue".to_string(), "Continue"));
    }
    let titles = options
        .iter()
        .map(|o| (o.option_id.0.to_string(), o.name.clone()))
        .collect();
    (options, titles)
}

fn permission_option(id: String, title: &str) -> PermissionOption {
    PermissionOption::new(id, title.to_string(), PermissionOptionKind::AllowOnce)
}

/// Map a `session/request_permission` outcome back to an `ask_user` reply.
fn ask_reply_from_outcome(
    outcome: &Result<RequestPermissionResponse, agent_client_protocol::Error>,
    option_titles: &[(String, String)],
) -> AskUserResponse {
    match outcome {
        Ok(r) => match &r.outcome {
            RequestPermissionOutcome::Selected(sel) => {
                let title = option_titles
                    .iter()
                    .find(|(id, _)| id == &sel.option_id.0.to_string())
                    .map(|(_, t)| t.clone())
                    .unwrap_or_else(|| sel.option_id.0.to_string());
                AskUserResponse::Answer(title)
            }
            _ => AskUserResponse::Cancelled,
        },
        Err(_) => AskUserResponse::Cancelled,
    }
}

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

fn render_prompt_blocks(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(img) => {
                parts.push(format!(
                    "data:{};base64,{}\n[Image attached by client]",
                    img.mime_type, img.data
                ));
            }
            ContentBlock::ResourceLink(r) => parts.push(format!("(see attached: {})", r.uri)),
            ContentBlock::Resource(r) => parts.push(format!(
                "(see attached: {})",
                match &r.resource {
                    agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(t) => t.uri.clone(),
                    agent_client_protocol::schema::v1::EmbeddedResourceResource::BlobResourceContents(b) => b.uri.clone(),
                    _ => String::new(),
                }
            )),
            _ => parts.push("(unsupported content block)".to_string()),
        }
    }
    parts.join("\n\n")
}

fn resource_text(
    r: &agent_client_protocol::schema::v1::EmbeddedResourceResource,
) -> Option<String> {
    match r {
        agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(t) => {
            Some(t.text.clone())
        }
        _ => None,
    }
}

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

fn resource_uri(r: &agent_client_protocol::schema::v1::EmbeddedResourceResource) -> String {
    match r {
        agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(t) => {
            t.uri.clone()
        }
        agent_client_protocol::schema::v1::EmbeddedResourceResource::BlobResourceContents(b) => {
            b.uri.clone()
        }
        _ => String::new(),
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// On-disk shape of an ACP session (events + cwd), written after each prompt
/// so a later process can resume the conversation via `session/load`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedSession {
    id: String,
    cwd: PathBuf,
    updated: u64,
    events: Vec<SessionEvent>,
}

/// Namespaced sessions dir, separate from the TUI's `sessions/<cwd>/` store.
fn acp_sessions_dir() -> Option<PathBuf> {
    crate::dirs::PROJECT_DIRS
        .as_ref()
        .map(|d| d.data_dir().join("sessions").join("acp"))
}

/// Atomically write the session's committed history + cwd to disk.
fn persist_session(id: &SessionId, events: &[SessionEvent], cwd: &Path) {
    let Some(dir) = acp_sessions_dir() else {
        return;
    };
    persist_session_in(&dir, id, events, cwd);
}

fn persist_session_in(dir: &Path, id: &SessionId, events: &[SessionEvent], cwd: &Path) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let payload = PersistedSession {
        id: id.to_string(),
        cwd: cwd.to_path_buf(),
        updated: now_ts(),
        events: events.to_vec(),
    };
    let Ok(json) = serde_json::to_string_pretty(&payload) else {
        return;
    };
    let path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!("{id}.json.tmp"));
    // Write to a temp name first so a crash never leaves a torn session file.
    if fs::write(&tmp, json.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Read a persisted session, if present.
fn read_persisted_session(id: &SessionId) -> Option<PersistedSession> {
    let dir = acp_sessions_dir()?;
    read_persisted_session_in(&dir, id)
}

fn read_persisted_session_in(dir: &Path, id: &SessionId) -> Option<PersistedSession> {
    if !dir.is_dir() {
        return None;
    }
    let data = fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&data).ok()
}

/// All persisted sessions, newest `updated` first.
fn persisted_session_list() -> Vec<PersistedSession> {
    let Some(dir) = acp_sessions_dir() else {
        return Vec::new();
    };
    list_persisted_in(&dir)
}

/// Remove a persisted session from disk. Returns `true` if a file was deleted.
fn delete_persisted_session(id: &SessionId) -> bool {
    let Some(dir) = acp_sessions_dir() else {
        return false;
    };
    let path = dir.join(format!("{id}.json"));
    let existed = path.is_file();
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(dir.join(format!("{id}.json.tmp")));
    existed
}

fn list_persisted_in(dir: &Path) -> Vec<PersistedSession> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PersistedSession> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            let data = fs::read_to_string(e.path()).ok()?;
            serde_json::from_str(&data).ok()
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.updated));
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn usage_update_value(usage: &UsageStats, context_size: u64) -> Option<UsageUpdate> {
    let used = usage.used_tokens()? as u64;
    Some(UsageUpdate::new(used.min(context_size), context_size))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tool_kind_names() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("find_files"), ToolKind::Search);
        assert_eq!(tool_kind("mystery_tool"), ToolKind::Other);
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

    #[test]
    fn thinking_round_trip() {
        assert_eq!(ThinkingLevel::parse("high"), Some(ThinkingLevel::High));
        assert_eq!(ThinkingLevel::parse("nope"), None);
        assert_eq!(ThinkingLevel::High.as_str(), "high");
    }

    // ── ask_user → request_permission mapping ────────────────────────────────

    use crate::agent::types::AskUserOption;

    fn ask_request_opts(
        options: Vec<AskUserOption>,
        allow_freeform: bool,
        allow_multiple: bool,
    ) -> (AskRequest, tokio::sync::oneshot::Receiver<AskUserResponse>) {
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        (
            AskRequest {
                question: "proceed?".to_string(),
                context: None,
                options,
                allow_multiple,
                allow_freeform,
                reply,
            },
            reply_rx,
        )
    }

    fn ask_request(
        options: Vec<AskUserOption>,
    ) -> (AskRequest, tokio::sync::oneshot::Receiver<AskUserResponse>) {
        ask_request_opts(options, true, false)
    }

    #[test]
    fn permission_options_map_titles_descriptions_and_continue_escape() {
        // Freeform asks add a trailing "Continue" escape even when options exist.
        let (req, _) = ask_request_opts(
            vec![
                AskUserOption {
                    title: "Yes".to_string(),
                    description: None,
                },
                AskUserOption {
                    title: "No".to_string(),
                    description: None,
                },
            ],
            true,
            false,
        );
        let (opts, titles) = permission_options(&req);
        assert_eq!(opts.len(), 3, "freeform ask: 2 options + Continue");
        assert_eq!(opts[0].name, "Yes");
        assert_eq!(opts[1].name, "No");
        assert_eq!(opts[0].option_id.0.as_ref(), "opt-0");
        assert_eq!(titles[1].1, "No");
        assert_eq!(opts[2].name, "Continue");
        assert_eq!(titles[2].1, "Continue");

        // Non-freeform asks with options: no Continue injection.
        let (req, _) = ask_request_opts(
            vec![AskUserOption {
                title: "Run tests".to_string(),
                description: Some("cargo test --all-features".to_string()),
            }],
            false,
            false,
        );
        let (opts, _) = permission_options(&req);
        assert_eq!(opts.len(), 1, "non-freeform ask: no Continue escape");
        assert_eq!(
            opts[0].name, "Run tests — cargo test --all-features",
            "description folded into visible label"
        );

        // Empty options degenerate to a Continue escape in all cases.
        let (bare, _) = ask_request_opts(vec![], false, false);
        let (opts, titles) = permission_options(&bare);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].name, "Continue");
        assert_eq!(titles[0].1, "Continue");
        assert_eq!(opts[0].option_id.0.as_ref(), "continue");
    }

    #[test]
    fn ask_reply_maps_selected_cancelled_and_error_outcomes() {
        use agent_client_protocol::schema::v1::{
            PermissionOptionId as Id, SelectedPermissionOutcome,
        };

        let (req, _) = ask_request(vec![AskUserOption {
            title: "Run tests".to_string(),
            description: None,
        }]);
        let (_opts, titles) = permission_options(&req);

        let selected = Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(Id::new("opt-0"))),
        ));
        assert!(matches!(
            ask_reply_from_outcome(&selected, &titles),
            AskUserResponse::Answer(t) if t == "Run tests"
        ));

        let cancelled = Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
        assert!(matches!(
            ask_reply_from_outcome(&cancelled, &titles),
            AskUserResponse::Cancelled
        ));

        let err = Err(agent_client_protocol::Error::internal_error());
        assert!(matches!(
            ask_reply_from_outcome(&err, &titles),
            AskUserResponse::Cancelled
        ));
    }

    #[test]
    fn ask_reply_falls_back_to_option_id_for_unknown_selection() {
        use agent_client_protocol::schema::v1::{
            PermissionOptionId as Id, SelectedPermissionOutcome,
        };
        let unknown = Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(Id::new("mystery"))),
        ));
        let titles = [("opt-0".to_string(), "Run tests".to_string())];
        assert!(matches!(
            ask_reply_from_outcome(&unknown, &titles),
            AskUserResponse::Answer(t) if t == "mystery"
        ));
    }

    // ── disk persistence round-trip ──────────────────────────────────────────

    #[test]
    fn session_persistence_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("ri-acp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = SessionId::new("sess-resume-test".to_string());
        let events = vec![
            SessionEvent::UserMessage {
                content: "acp devam".to_string(),
                timestamp: 100,
            },
            SessionEvent::AssistantMessage {
                content: "merhaba".to_string(),
                thinking: None,
                phase: AssistantPhase::Final,
                usage: None,
                timestamp: 101,
            },
        ];
        persist_session_in(&dir, &id, &events, Path::new("/tmp/work"));

        let loaded = read_persisted_session_in(&dir, &id).expect("load persisted");
        assert_eq!(loaded.cwd, PathBuf::from("/tmp/work"));
        assert_eq!(loaded.events.len(), 2);
        assert!(matches!(
            &loaded.events[1],
            SessionEvent::AssistantMessage { content, .. } if content == "merhaba"
        ));

        let list = list_persisted_in(&dir);
        assert_eq!(list.len(), 1, "expected one persisted session");
        assert_eq!(list[0].cwd, PathBuf::from("/tmp/work"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
