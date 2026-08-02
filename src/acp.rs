//! ACP (Agent Client Protocol) server — headless operation of `ri-agent`.
//!
//! Runs the existing multi-turn agent loop behind the vendor-neutral ACP
//! JSON-RPC surface, so any ACP-capable client (editors, desktop/web UIs,
//! `acpx`, …) can drive ri as a subprocess (stdio) or over WebSocket.
//!
//! Implemented surface:
//! - `initialize` — protocol negotiation (image prompts, `session/load`),
//!   advertises `session/fork` (ACP v2 session capability)
//! - `session/new` — in-memory session (history, cancel channel, cwd)
//! - `session/load` — replays a known session's history as updates; sessions
//!   are persisted to disk (`~/.local/share/ri/sessions/acp/<id>.json`) after
//!   each prompt, so they can be resumed by a later process
//! - `session/fork` — clones a session (in-memory or persisted) into a new id
//!   so clients can branch conversations
//! - `session/prompt` — streams `agent_message_chunk`, `agent_thought_chunk`,
//!   `tool_call`/`tool_call_update` (with live tool output forwarded as
//!   in-progress `tool_call_update` chunks), `usage_update`, then `end_turn`;
//!   tools run anchored at the session `cwd`, auto-compaction is enabled, and
//!   the final `end_turn` response folds the turn's token usage
//!   (`end_turn_token_usage`)
//! - `session/cancel` — maps to ri `HardAbort`
//! - `ask_user` → `session/request_permission` (multiple-choice mapping;
//!   option descriptions folded into labels; freeform asks surface a trailing
//!   "Continue" escape)
//! - Custom `_ri/*` methods: `_ri/get_state`, `_ri/set_model`,
//!   `_ri/set_thinking`, `_ri/set_provider` (re-resolves a provider preset by
//!   id — the provider is rebuilt and the "current instance" + model updated),
//!   `_ri/list_sessions` / `_ri/delete_session` / `_ri/prune_sessions`
//!   (persisted-session management), `_ri/logs` (recent activity),
//!   `_ri/steering` (queues steering for the *next* prompt turn). Mutating
//!   `_ri/*` methods require an admin `token` when `--serve-ws-token` is set.
//!
//! Transports: stdio (`--serve`), or HTTP + WebSocket at `/acp` (`--serve-ws <ADDR>`,
//! optionally TLS via `--serve-ws-cert`/`--serve-ws-key`). The WebSocket server
//! multiplexes multiple client connections; stdio serves exactly one.
//!
//! Protocol negotiations: each connection's `initialize` is routed through an
//! `AgentProtocolRouter`. Protocol v1 serves the full surface (load/close/
//! list/fork + `_ri/*`); protocol **v2** (unstable) is served over the same
//! per-turn core with the standard v2 surface — `initialize`, `session/new`,
//! `session/resume` (register + replay), `session/list`, `session/close`,
//! `session/fork`, `session/delete`, `session/prompt` (streamed v2
//! `UpdateSessionNotification` chunks with per-turn `MessageId`, plus embedded
//! resource reads), `session/cancel`, and `ask_user` →
//! `session/request_permission` (v2 title+options), and the ri-specific
//! `_ri/*` custom methods (shared version-neutral implementations registered
//! on both the v1 and v2 agents).
//!
//! Event loop: `session/prompt` handlers return immediately and run the whole
//! turn (event forwarding, the `request_permission` ask bridge, the final
//! response) as a concurrent task with the `Responder` moved in. That keeps the
//! SDK event loop free during a run, so `_ri/get_state` gives a **live** mid-turn
//! snapshot and `session/cancel` is dispatched while streaming. Known
//! limitations: one prompt at a time per session; `_ri/steering` still applies
//! at the next turn boundary rather than mid-turn; and the agent loop's
//! `stream_assistant_turn` does not yet observe `HardAbort` between tokens, so
//! cancel takes effect at the next checkpoint (turn/tool boundary) rather than
//! chopping a stream in flight. The agent loop itself is reused unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::{fs, path::Path};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, ForkSessionRequest, ForkSessionResponse, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionForkCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    StopReason, TextContent, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, Usage as AcpUsage, UsageUpdate,
};
use agent_client_protocol::schema::v2 as acp_v2;
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, JsonRpcRequest, JsonRpcResponse, Responder,
    Result as AcpResult, Stdio, on_receive_notification, on_receive_request,
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
    #[serde(default)]
    token: Option<String>,
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
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiSetThinkingResponse {
    ok: bool,
    error: Option<String>,
    level: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/set_provider", response = RiSetProviderResponse)]
struct RiSetProviderRequest {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiSetProviderResponse {
    ok: bool,
    error: Option<String>,
    model: String,
    thinking: String,
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
    #[serde(default)]
    token: Option<String>,
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
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiPruneSessionsResponse {
    ok: bool,
    deleted: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/steering", response = RiSteeringResponse)]
struct RiSteeringRequest {
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiSteeringResponse {
    ok: bool,
    error: Option<String>,
    /// True when the text was queued on the session for the next turn.
    forwarded: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_ri/logs", response = RiLogsResponse)]
struct RiLogsRequest {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
struct RiLogsResponse {
    logs: Vec<AcpLogEntry>,
}

// ── Context ───────────────────────────────────────────────────────────────────

/// Rebuilds the active provider for a given provider id + model + thinking
/// level (used by the `_ri/set_model` / `_ri/set_thinking` /
/// `_ri/set_provider` custom methods). The provider id is `None` to rebuild
/// on the current instance; `Some(id)` re-resolves the provider preset by id
/// and updates the server's "current instance". Returns the new provider and
/// its effective model name.
pub type ProviderRebuild = Arc<
    dyn Fn(
            Option<&str>,
            &str,
            ThinkingLevel,
        ) -> anyhow::Result<(Arc<dyn LlmProvider + Send + Sync>, String)>
        + Send
        + Sync,
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
    /// Bounded activity buffer surfaced by `_ri/logs`.
    pub logs: Arc<Mutex<std::collections::VecDeque<AcpLogEntry>>>,
    /// Optional admin token. When set, state-mutating `_ri/*` methods require
    /// a matching `token` request field (`--serve-ws-token`).
    pub admin_token: Option<Arc<str>>,
}

/// A single `_ri/logs` entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpLogEntry {
    ts: u64,
    level: String,
    session: Option<String>,
    message: String,
}

impl AcpContext {
    /// Append a bounded activity entry (surfaced via `_ri/logs`).
    pub fn log(&self, level: &str, session: Option<&str>, message: impl Into<String>) {
        let mut logs = self.logs.lock().unwrap();
        logs.push_back(AcpLogEntry {
            ts: now_ts(),
            level: level.to_string(),
            session: session.map(str::to_string),
            message: message.into(),
        });
        if logs.len() > 500 {
            logs.pop_front();
        }
    }

    /// Allow the request when no admin token is configured, or when the
    /// provided token matches the configured one.
    pub fn authorize(&self, token: Option<&str>) -> bool {
        match &self.admin_token {
            Some(expected) => token.is_some_and(|t| t == expected.as_ref()),
            None => true,
        }
    }
}

/// In-memory per-session state.
struct AcpSession {
    events: Vec<SessionEvent>,
    running: bool,
    cancel_tx: Option<watch::Sender<CancelLevel>>,
    /// Steering messages queued via `_ri/steering`. Because the SDK dispatches
    /// requests serially, mid-turn injection is impossible; the queued texts
    /// are fed into the next prompt's turn boundary instead (turn-boundary
    /// steering semantics).
    queued_steering: Vec<String>,
    #[allow(dead_code)] // kept for future tool-cwd wiring; session prompt uses it
    cwd: std::path::PathBuf,
}

impl AcpSession {
    fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            events: Vec::new(),
            running: false,
            cancel_tx: None,
            queued_steering: Vec::new(),
            cwd,
        }
    }
}

type Sessions = Arc<Mutex<HashMap<SessionId, AcpSession>>>;

macro_rules! send_update {
    ($connection:expr, $session_id:expr, $update:expr) => {{
        let _ =
            $connection.send_notification(SessionNotification::new($session_id.clone(), $update));
    }};
}

// ── Shared turn driver (protocol-version-agnostic) ───────────────────────────

/// Everything the per-turn driver needs, produced synchronously by
/// [`prepare_turn`] and consumed (on a concurrent task) by [`run_turn`].
/// Deliberately independent of the ACP protocol version.
struct TurnSetup {
    context_size: usize,
    join: tokio::task::JoinHandle<()>,
    rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    cancel_rx: tokio::sync::watch::Receiver<CancelLevel>,
}

/// Version-agnostic summary of one completed prompt turn.
struct TurnOutcome {
    error: Option<String>,
    final_usage: Option<UsageStats>,
}

/// Protocol-version-specific mapping of the shared event-forwarding loop onto
/// ACP notifications, permission requests, and the final prompt response.
/// The v1 and v2 implementations translate the same [`AppEvent`] stream into
/// the appropriate schema types; the caller passes the session id as a plain
/// string so the trait itself stays version-neutral.
trait TurnEmitter: Send + 'static {
    fn emit_agent_text(&self, sid: &str, text: &str);
    fn emit_agent_thought(&self, sid: &str, text: &str);
    fn emit_usage(&self, sid: &str, u: &UsageStats, context_size: usize);
    fn emit_tool_pending(&self, sid: &str, id: &str, name: &str);
    fn emit_tool_completed(&self, sid: &str, id: &str, content: &str);
    fn emit_tool_output_chunk(&self, sid: &str, id: &str, chunk: &str);
    async fn ask(
        &mut self,
        sid: &str,
        request: &AskRequest,
        cancel_rx: &mut tokio::sync::watch::Receiver<CancelLevel>,
    ) -> AskUserResponse;
    /// Send the version-specific `session/prompt` response (or error).
    fn finish(self, sid: &str, outcome: TurnOutcome);
}

/// Shared permission-option listing `(id, title)` for an ask, ending with the
/// freeform "Continue" escape when the ask allows freeform input or has no
/// options. Each protocol version maps these into its own `PermissionOption` list.
fn permission_option_rows(request: &AskRequest) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = request
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let label = match o.description.as_deref() {
                Some(d) if !d.is_empty() => format!("{} — {d}", o.title),
                _ => o.title.clone(),
            };
            (format!("opt-{i}"), label)
        })
        .collect();
    if request.allow_freeform || rows.is_empty() {
        rows.push(("continue".to_string(), "Continue".to_string()));
    }
    rows
}

/// Turn a selected option id into the `ask_user` reply (prefer the option
/// title, fall back to the id). `None` (no selection, cancelled) maps to
/// [`AskUserResponse::Cancelled`].
fn answer_from_selected(option_id: Option<&str>, rows: &[(String, String)]) -> AskUserResponse {
    match option_id {
        Some(id) => {
            let title = rows
                .iter()
                .find(|(oid, _)| oid == id)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| id.to_string());
            AskUserResponse::Answer(title)
        }
        None => AskUserResponse::Cancelled,
    }
}

/// Reserve a session for a prompt turn (busy guard, user-message + resource
/// reads into history, cancel + steering channels) and spawn the agent loop.
/// Version-agnostic apart from the already-rendered `prompt_text` and
/// `resource_reads` supplied by the caller.
async fn prepare_turn(
    ctx: &Arc<AcpContext>,
    s_prompt: &Sessions,
    session_id: &SessionId,
    prompt_text: String,
    resource_reads: Vec<SessionEvent>,
) -> Result<TurnSetup, String> {
    let (session_cwd, context_size, history_snapshot, steering_rx, cancel_rx, cancel_rx_turn) = {
        let mut map = s_prompt.lock().unwrap();
        let Some(s) = map.get_mut(session_id) else {
            return Err(format!("unknown session: {session_id}"));
        };
        if s.running {
            return Err("session is busy (one prompt at a time)".to_string());
        }
        s.running = true;
        s.events.push(SessionEvent::UserMessage {
            content: prompt_text.clone(),
            timestamp: now_ts(),
        });
        s.events.extend(resource_reads);
        let (cancel_tx, cancel_rx) = watch::channel(CancelLevel::None);
        s.cancel_tx = Some(cancel_tx);
        let (steering_tx, steering_rx) = tokio::sync::mpsc::unbounded_channel();
        for text in s.queued_steering.drain(..) {
            let _ = steering_tx.send(text);
        }
        let cancel_rx_turn = cancel_rx.clone();
        (
            s.cwd.clone(),
            context_window_for_model(&ctx.model.read().unwrap()).unwrap_or(200_000),
            s.events.clone(),
            steering_rx,
            cancel_rx,
            cancel_rx_turn,
        )
    };

    // Channel for agent events AND ask_user replies.
    let (tx, rx): (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) =
        tokio::sync::mpsc::unbounded_channel();
    // Tools are registered per prompt so `ask_user` can route its question back
    // through this prompt's channel (headless mode).
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
        executor: Arc::new(DefaultToolExecutor::with_root(session_cwd)),
        system_prompt: Some(system_prompt),
    };
    let provider_loop = Arc::clone(&provider);
    let task_tx = tx.clone();
    let join = tokio_handle.spawn(async move {
        run_agent_loop(config, provider_loop, task_tx, steering_rx, cancel_rx).await;
    });
    Ok(TurnSetup {
        context_size,
        join,
        rx,
        cancel_rx: cancel_rx_turn,
    })
}

/// Forward the agent's events to the client via the version-specific [`TurnEmitter`],
/// answer `ask_user` through a permission request, then finalize (busy-guard
/// reset, history persistence, response). Runs on its own task so the SDK
/// event loop stays free during the turn.
async fn run_turn<E: TurnEmitter>(
    ctx: Arc<AcpContext>,
    s_prompt: Sessions,
    session_id: SessionId,
    setup: TurnSetup,
    mut emitter: E,
) {
    let TurnSetup {
        context_size,
        join,
        mut rx,
        cancel_rx,
    } = setup;
    let mut cancel_rx = cancel_rx;
    let sid = session_id.to_string();
    let mut error: Option<String> = None;
    let mut assistant_text = String::new();
    let mut assistant_thinking: Option<String> = None;
    let mut phase = AssistantPhase::Unknown;
    let mut usage: Option<UsageStats> = None;
    // Usage survives TurnEnd's `usage = None` reset so the final end_turn
    // response can include the last turn's tokens.
    let mut final_usage: Option<UsageStats> = None;
    let mut pending_tool: Vec<SessionEvent> = Vec::new();

    while let Some(ev) = rx.recv().await {
        match ev {
            AppEvent::AskUser(request) => {
                let answer = emitter.ask(&sid, &request, &mut cancel_rx).await;
                match &answer {
                    AskUserResponse::Answer(a) => {
                        ctx.log("info", Some(&sid), format!("ask_user answered: {a}"))
                    }
                    AskUserResponse::Cancelled => ctx.log("warn", Some(&sid), "ask_user cancelled"),
                }
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
                    emitter.emit_agent_text(&sid, &text);
                }
                AgentEvent::ThinkingToken(t) => {
                    assistant_thinking
                        .get_or_insert_with(String::new)
                        .push_str(&t);
                    emitter.emit_agent_thought(&sid, &t);
                }
                AgentEvent::Usage(u) => {
                    usage = Some(u);
                    final_usage = Some(u);
                    emitter.emit_usage(&sid, &u, context_size);
                }
                AgentEvent::ToolCallStart { id, name, args } => {
                    pending_tool.push(SessionEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        args,
                        include_in_llm: true,
                        timestamp: now_ts(),
                    });
                    emitter.emit_tool_pending(&sid, &id, &name);
                }
                AgentEvent::ToolCallEnd { id, result } => {
                    let content = result.content.as_text().to_string();
                    pending_tool.push(SessionEvent::ToolResult {
                        id: id.clone(),
                        name: String::new(),
                        content: content.clone(),
                        is_error: result.is_error,
                        display_range: None,
                        include_in_llm: true,
                        timestamp: now_ts(),
                    });
                    emitter.emit_tool_completed(&sid, &id, &content);
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
                    emitter.emit_tool_output_chunk(&sid, &id, &chunk);
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

    // Persist the committed history + cwd so the session can be resumed from
    // disk by a later process (session/load).
    {
        let map = s_prompt.lock().unwrap();
        if let Some(s) = map.get(&session_id)
            && !s.events.is_empty()
        {
            persist_session(&session_id, &s.events, &s.cwd);
        }
    }

    match &error {
        Some(msg) => ctx.log("error", Some(&sid), format!("session/prompt failed: {msg}")),
        None => ctx.log("info", Some(&sid), "session/prompt end"),
    }
    emitter.finish(&sid, TurnOutcome { error, final_usage });
}

// ── ACP protocol v1 emitter ──────────────────────────────────────────────────

/// v1 transport of the shared turn driver: maps agent events onto v1
/// `SessionNotification`/`SessionUpdate`, asks via v1
/// `session/request_permission`, and finishes with the v1 `PromptResponse`.
struct V1Emitter {
    connection: ConnectionTo<Client>,
    responder: Responder<PromptResponse>,
}

impl V1Emitter {
    fn new(connection: ConnectionTo<Client>, responder: Responder<PromptResponse>) -> Self {
        Self {
            connection,
            responder,
        }
    }

    fn send_update(&self, sid: &str, update: SessionUpdate) {
        send_update!(self.connection, SessionId::new(sid.to_string()), update);
    }
}

impl TurnEmitter for V1Emitter {
    fn emit_agent_text(&self, sid: &str, text: &str) {
        self.send_update(
            sid,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        );
    }

    fn emit_agent_thought(&self, sid: &str, text: &str) {
        self.send_update(
            sid,
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        );
    }

    fn emit_usage(&self, sid: &str, u: &UsageStats, context_size: usize) {
        if let Some(uu) = usage_update_value(u, context_size as u64) {
            self.send_update(sid, SessionUpdate::UsageUpdate(uu));
        }
    }

    fn emit_tool_pending(&self, sid: &str, id: &str, name: &str) {
        let kind = tool_kind(name);
        self.send_update(
            sid,
            SessionUpdate::ToolCall(
                ToolCall::new(id.to_string(), name.to_string())
                    .kind(kind)
                    .status(ToolCallStatus::Pending),
            ),
        );
    }

    fn emit_tool_completed(&self, sid: &str, id: &str, content: &str) {
        self.send_update(
            sid,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_string(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(content),
                    ))]),
            )),
        );
    }

    fn emit_tool_output_chunk(&self, sid: &str, id: &str, chunk: &str) {
        // Live tool output: stream each chunk as an in-progress
        // `tool_call_update` so headless clients render bash/exec output as it
        // runs (the final `Completed` update arrives on `ToolCallEnd`).
        self.send_update(
            sid,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_string(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(chunk),
                    ))]),
            )),
        );
    }

    async fn ask(
        &mut self,
        sid: &str,
        request: &AskRequest,
        cancel_rx: &mut tokio::sync::watch::Receiver<CancelLevel>,
    ) -> AskUserResponse {
        let rows = permission_option_rows(request);
        let mut title = request.question.clone();
        if let Some(ctx_text) = request.context.as_deref() {
            title = format!("{title}\n\n{ctx_text}");
        }
        let tool_call = ToolCallUpdate::new(
            format!("ask-{}", now_ts()),
            ToolCallUpdateFields::new()
                .title(title)
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress),
        );
        let options = rows
            .iter()
            .map(|(id, t)| permission_option(id.clone(), t))
            .collect();
        // Safe to await block_task here: this task runs concurrently with the
        // (free) event loop, so the client's response is routed to us. A
        // mid-turn session/cancel also resolves the ask (as cancelled)
        // instead of hanging.
        let outcome = tokio::select! {
            o = self.connection.send_request(RequestPermissionRequest::new(
                SessionId::new(sid.to_string()),
                tool_call,
                options,
            ))
            .block_task() => Some(o),
            _ = cancel_rx.wait_for(|l| *l >= CancelLevel::HardAbort) => None,
        };
        match outcome {
            Some(outcome) => match &outcome {
                Ok(r) => match &r.outcome {
                    RequestPermissionOutcome::Selected(sel) => {
                        answer_from_selected(Some(sel.option_id.0.as_ref()), &rows)
                    }
                    _ => AskUserResponse::Cancelled,
                },
                Err(_) => AskUserResponse::Cancelled,
            },
            None => AskUserResponse::Cancelled,
        }
    }

    fn finish(self, _sid: &str, outcome: TurnOutcome) {
        let _ = match outcome.error {
            Some(msg) => self
                .responder
                .respond_with_error(agent_client_protocol::Error::internal_error().data(msg)),
            None => {
                let mut resp = PromptResponse::new(StopReason::EndTurn);
                // ACP end_turn_token_usage: fold the turn's usage into the
                // response in addition to the streamed usage_update.
                if let Some(u) = &outcome.final_usage {
                    resp = resp.usage(AcpUsage::new(
                        u.total_tokens.unwrap_or_default() as u64,
                        u.input_tokens.unwrap_or_default() as u64,
                        u.output_tokens.unwrap_or_default() as u64,
                    ));
                }
                self.responder.respond(resp)
            }
        };
    }
}

/// Reject an unauthorized mutating `_ri/*` request and return from the handler.
macro_rules! unauthorized {
    ($responder:expr, $method:expr) => {{
        $responder
            .respond_with_error(agent_client_protocol::Error::invalid_params().data(format!(
                "{}.unauthorized: invalid or missing admin token",
                $method
            )))
            .ok();
        return Ok(());
    }};
}

// ── Shared `_ri/*` implementations ────────────────────────────────────────────
// The ri-specific admin surface is protocol-version-neutral: the same logic is
// registered on both the v1 and the v2 agents (auth is enforced in the thin
// transport closures via the shared `unauthorized!` macro).

fn ri_get_state(ctx: &AcpContext, sessions: &Sessions) -> RiGetStateResponse {
    let model = ctx.model.read().unwrap().clone();
    let thinking = ctx.thinking.read().unwrap().as_str().to_string();
    let map = sessions.lock().unwrap();
    let session_ids = map.keys().map(|s| s.to_string()).collect::<Vec<_>>();
    let streaming = map.values().filter(|s| s.running).count();
    RiGetStateResponse {
        model,
        thinking,
        sessions: session_ids,
        streaming_sessions: streaming,
    }
}

fn ri_list_sessions() -> RiListSessionsResponse {
    let sessions = persisted_session_list()
        .into_iter()
        .map(|p| RiSessionMeta {
            id: p.id,
            cwd: p.cwd.to_string_lossy().into_owned(),
            updated: p.updated,
        })
        .collect();
    RiListSessionsResponse { sessions }
}

fn ri_steering(
    sessions: &Sessions,
    session_id: &str,
    text: &str,
    ctx: &AcpContext,
) -> RiSteeringResponse {
    if session_id.trim().is_empty() || text.trim().is_empty() {
        return RiSteeringResponse {
            ok: false,
            error: Some("sessionId and text are required".to_string()),
            forwarded: false,
        };
    }
    let forwarded = {
        let mut map = sessions.lock().unwrap();
        if let Some(s) = map.get_mut(&SessionId::new(session_id.to_string())) {
            s.queued_steering.push(text.to_string());
            true
        } else {
            false
        }
    };
    ctx.log(
        if forwarded { "info" } else { "warn" },
        Some(session_id),
        if forwarded {
            "_ri/steering queued for next turn".to_string()
        } else {
            "unknown session for _ri/steering".to_string()
        },
    );
    RiSteeringResponse {
        ok: forwarded,
        error: if forwarded {
            None
        } else {
            Some("unknown session".to_string())
        },
        forwarded,
    }
}

fn ri_delete_session(
    sessions: &Sessions,
    session_id: String,
    ctx: &AcpContext,
) -> RiDeleteSessionResponse {
    if session_id.trim().is_empty() {
        return RiDeleteSessionResponse {
            ok: false,
            error: Some("sessionId must not be empty".to_string()),
            deleted_in_memory: false,
            deleted_on_disk: false,
        };
    }
    let id = SessionId::new(session_id.clone());
    let mut deleted_in_memory = false;
    let mut error: Option<String> = None;
    {
        let mut map = sessions.lock().unwrap();
        if let Some(s) = map.get(&id) {
            if s.running {
                error = Some("session is busy; cancel the active prompt first".to_string());
            } else {
                map.remove(&id);
                deleted_in_memory = true;
            }
        }
    }
    if error.is_some() {
        return RiDeleteSessionResponse {
            ok: false,
            error,
            deleted_in_memory,
            deleted_on_disk: false,
        };
    }
    let deleted_on_disk = delete_persisted_session(&id);
    ctx.log(
        "info",
        Some(&session_id),
        format!("_ri/delete_session (memory={deleted_in_memory}, disk={deleted_on_disk})"),
    );
    RiDeleteSessionResponse {
        ok: true,
        error: None,
        deleted_in_memory,
        deleted_on_disk,
    }
}

fn ri_prune_sessions(ctx: &AcpContext, older_than: u64) -> RiPruneSessionsResponse {
    let cutoff = now_ts().saturating_sub(older_than);
    let mut deleted = 0usize;
    if let Some(dir) = acp_sessions_dir() {
        let Ok(entries) = fs::read_dir(&dir) else {
            return RiPruneSessionsResponse { ok: true, deleted };
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
    ctx.log(
        "info",
        None,
        format!("_ri/prune_sessions deleted {deleted}"),
    );
    RiPruneSessionsResponse { ok: true, deleted }
}

fn ri_set_model(ctx: &AcpContext, model: String) -> RiSetModelResponse {
    if model.trim().is_empty() {
        return RiSetModelResponse {
            ok: false,
            error: Some("model must not be empty".to_string()),
            model,
        };
    }
    let thinking = *ctx.thinking.read().unwrap();
    match (ctx.rebuild)(None, &model, thinking) {
        Ok((provider, _model)) => {
            *ctx.model.write().unwrap() = model.clone();
            *ctx.provider.write().unwrap() = provider;
            ctx.log("info", None, format!("_ri/set_model -> {model}"));
            RiSetModelResponse {
                ok: true,
                error: None,
                model,
            }
        }
        Err(e) => {
            ctx.log("error", None, format!("_ri/set_model failed: {e}"));
            RiSetModelResponse {
                ok: false,
                error: Some(e.to_string()),
                model,
            }
        }
    }
}

fn ri_set_thinking(ctx: &AcpContext, level: String) -> RiSetThinkingResponse {
    let Some(level) = ThinkingLevel::parse(&level) else {
        return RiSetThinkingResponse {
            ok: false,
            error: Some(format!("unknown thinking level '{level}'")),
            level,
        };
    };
    let model = ctx.model.read().unwrap().clone();
    match (ctx.rebuild)(None, &model, level) {
        Ok((provider, _model)) => {
            *ctx.thinking.write().unwrap() = level;
            *ctx.provider.write().unwrap() = provider;
            ctx.log(
                "info",
                None,
                format!("_ri/set_thinking -> {}", level.as_str()),
            );
            RiSetThinkingResponse {
                ok: true,
                error: None,
                level: level.as_str().to_string(),
            }
        }
        Err(e) => {
            ctx.log("error", None, format!("_ri/set_thinking failed: {e}"));
            RiSetThinkingResponse {
                ok: false,
                error: Some(e.to_string()),
                level: level.as_str().to_string(),
            }
        }
    }
}

fn ri_set_provider(ctx: &AcpContext, provider: String) -> RiSetProviderResponse {
    if provider.trim().is_empty() {
        return RiSetProviderResponse {
            ok: false,
            error: Some("provider must not be empty".to_string()),
            model: ctx.model.read().unwrap().clone(),
            thinking: ctx.thinking.read().unwrap().as_str().to_string(),
        };
    }
    let thinking = *ctx.thinking.read().unwrap();
    let current_model = ctx.model.read().unwrap().clone();
    match (ctx.rebuild)(Some(&provider), &current_model, thinking) {
        Ok((new_provider, model)) => {
            *ctx.model.write().unwrap() = model.clone();
            *ctx.provider.write().unwrap() = new_provider;
            ctx.log(
                "info",
                None,
                format!("_ri/set_provider -> {provider} (model {model})"),
            );
            RiSetProviderResponse {
                ok: true,
                error: None,
                model,
                thinking: thinking.as_str().to_string(),
            }
        }
        Err(e) => {
            ctx.log("error", None, format!("_ri/set_provider failed: {e}"));
            RiSetProviderResponse {
                ok: false,
                error: Some(e.to_string()),
                model: current_model,
                thinking: thinking.as_str().to_string(),
            }
        }
    }
}

fn ri_logs(ctx: &AcpContext, limit: Option<usize>) -> RiLogsResponse {
    let limit = limit.unwrap_or(200).min(1000);
    let most_recent_first: Vec<AcpLogEntry> = {
        let all = ctx.logs.lock().unwrap();
        all.iter().rev().take(limit).cloned().collect()
    };
    let logs = most_recent_first.into_iter().rev().collect();
    RiLogsResponse { logs }
}

/// Build the agent component (handlers registered) shared by the stdio and
/// WebSocket transports. Returns a builder that implements `ConnectTo<Client>`.
fn build_agent(
    ctx: Arc<AcpContext>,
    sessions: Sessions,
) -> impl agent_client_protocol::ConnectTo<Client> {
    let s_new = Arc::clone(&sessions);
    let s_load = Arc::clone(&sessions);
    let s_resume = Arc::clone(&sessions);
    let s_close = Arc::clone(&sessions);
    let s_list = Arc::clone(&sessions);
    let s_prompt = Arc::clone(&sessions);
    let s_cancel = Arc::clone(&sessions);
    let s_state = Arc::clone(&sessions);
    let s_del = Arc::clone(&sessions);
    let s_fork = Arc::clone(&sessions);
    let s_steer = Arc::clone(&sessions);
    let ctx_new = Arc::clone(&ctx);
    let ctx_fork = Arc::clone(&ctx);
    let ctx_steer = Arc::clone(&ctx);
    let ctx_resume = Arc::clone(&ctx);
    let ctx_close = Arc::clone(&ctx);
    let ctx_cancel = Arc::clone(&ctx);
    let ctx_state = Arc::clone(&ctx);
    let ctx_set_model = Arc::clone(&ctx);
    let ctx_set_thinking = Arc::clone(&ctx);
    let ctx_set_provider = Arc::clone(&ctx);
    let ctx_logs = Arc::clone(&ctx);
    let ctx_del = Arc::clone(&ctx);
    let ctx_prune = Arc::clone(&ctx);

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
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .resume(SessionResumeCapabilities::new())
                                    .close(SessionCloseCapabilities::new())
                                    .fork(SessionForkCapabilities::new()),
                            )
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
                ctx_new.log("info", Some(&id.to_string()), "session/new");
                responder.respond(NewSessionResponse::new(id))
            },
            on_receive_request!(),
        )
        // ── session/load (in-memory or disk resume) ───────────────────────
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, connection| {
                let (events, _cwd) = match resolve_session_for_load(&s_load, &req.session_id) {
                    SessionLoadResult::Ready { events, cwd } => (events, cwd),
                    SessionLoadResult::Busy => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("session is busy; cannot load while a prompt runs"),
                            )
                            .ok();
                        return Ok(());
                    }
                    SessionLoadResult::Unknown => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("unknown session"),
                            )
                            .ok();
                        return Ok(());
                    }
                };
                for update in session_replay_updates(&events) {
                    send_update!(connection, req.session_id, update);
                }
                responder.respond(LoadSessionResponse::new())
            },
            on_receive_request!(),
        )
        // ── session/resume (register + replay, sets the session as current) ─
        .on_receive_request(
            async move |req: ResumeSessionRequest, responder, connection| {
                let (events, _cwd) = match resolve_session_for_load(&s_resume, &req.session_id) {
                    SessionLoadResult::Ready { events, cwd } => (events, cwd),
                    SessionLoadResult::Busy => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("session is busy; cannot resume while a prompt runs"),
                            )
                            .ok();
                        return Ok(());
                    }
                    SessionLoadResult::Unknown => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("unknown session"),
                            )
                            .ok();
                        return Ok(());
                    }
                };
                ctx_resume.log("info", Some(&req.session_id.to_string()), "session/resume");
                for update in session_replay_updates(&events) {
                    send_update!(connection, req.session_id, update);
                }
                responder.respond(ResumeSessionResponse::new())
            },
            on_receive_request!(),
        )
        // ── session/close (end a session; history stays resumable on disk) ─
        .on_receive_request(
            async move |req: CloseSessionRequest, responder, _connection| {
                let mut map = s_close.lock().unwrap();
                if let Some(s) = map.get(&req.session_id)
                    && s.running
                {
                    drop(map);
                    responder
                        .respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("session is busy; cannot close while a prompt runs"),
                        )
                        .ok();
                    return Ok(());
                }
                let removed = map.remove(&req.session_id).is_some();
                drop(map);
                ctx_close.log(
                    "info",
                    Some(&req.session_id.to_string()),
                    if removed {
                        "session/close".to_string()
                    } else {
                        "session/close (already closed)".to_string()
                    },
                );
                responder.respond(CloseSessionResponse::new())
            },
            on_receive_request!(),
        )
        // ── session/list (vs persisted sessions, newest first) ─────────────
        .on_receive_request(
            async move |_req: ListSessionsRequest, responder, _connection| {
                let mut sessions: Vec<SessionInfo> = Vec::new();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                // Persisted first (they carry a recency timestamp).
                for p in persisted_session_list() {
                    seen.insert(p.id.clone());
                    sessions.push(
                        SessionInfo::new(p.id, p.cwd).updated_at(Some(unix_to_rfc3339(p.updated))),
                    );
                }
                // Then live in-memory sessions not yet listed.
                {
                    let map = s_list.lock().unwrap();
                    let mut live: Vec<(String, std::path::PathBuf)> = map
                        .iter()
                        .filter(|(id, _)| !seen.contains(&id.to_string()))
                        .map(|(id, s)| (id.to_string(), s.cwd.clone()))
                        .collect();
                    live.sort();
                    for (id, cwd) in live {
                        sessions.push(SessionInfo::new(SessionId::new(id), cwd));
                    }
                }
                responder.respond(ListSessionsResponse::new(sessions))
            },
            on_receive_request!(),
        )
        // ── session/fork (ACP v2 / unstable_session_fork) ─────────────────
        .on_receive_request(
            async move |req: ForkSessionRequest, responder, _connection| {
                // Source is the live in-memory session, then a persisted one.
                let src = {
                    let map = s_fork.lock().unwrap();
                    map.get(&req.session_id)
                        .map(|s| (s.events.clone(), s.cwd.clone()))
                };
                let (events, cwd) = match src {
                    Some(x) => x,
                    None => match read_persisted_session(&req.session_id) {
                        Some(p) => (p.events, p.cwd),
                        None => {
                            responder
                                .respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data("unknown session"),
                                )
                                .ok();
                            return Ok(());
                        }
                    },
                };

                let new_id = SessionId::new(format!("sess-{}", new_session_suffix()));
                let mut session = AcpSession::new(cwd.clone());
                session.events = events.clone();
                {
                    let mut map = s_fork.lock().unwrap();
                    map.insert(new_id.clone(), session);
                }
                persist_session(&new_id, &events, &cwd);
                ctx_fork.log(
                    "info",
                    Some(&req.session_id.to_string()),
                    format!("session/fork -> {new_id} ({} events)", events.len()),
                );
                responder.respond(ForkSessionResponse::new(new_id))
            },
            on_receive_request!(),
        )
        // ── session/prompt ─────────────────────────────────────────────────
        .on_receive_request(
            async move |req: PromptRequest, responder, connection| {
                let session_id = req.session_id.clone();
                let prompt_text = render_prompt_blocks(&req.prompt);
                ctx.log(
                    "info",
                    Some(&session_id.to_string()),
                    format!(
                        "session/prompt start ({})",
                        prompt_text.chars().take(60).collect::<String>()
                    ),
                );

                let resource_reads = synthesize_resource_reads(&req.prompt);
                let tokio_handle = ctx
                    .tokio_handle
                    .clone()
                    .unwrap_or_else(tokio::runtime::Handle::current);
                let setup =
                    match prepare_turn(&ctx, &s_prompt, &session_id, prompt_text, resource_reads)
                        .await
                    {
                        Ok(s) => s,
                        Err(msg) => {
                            responder
                                .respond_with_error(
                                    agent_client_protocol::Error::invalid_params().data(msg),
                                )
                                .ok();
                            return Ok(());
                        }
                    };

                // SDK event-loop constraint: handler callbacks run on a single
                // event loop, so the connection cannot read client responses
                // (including the reply to our session/request_permission)
                // while THIS handler is still executing. Run the whole turn
                // (event forwarding, the ask bridge, the final session/prompt
                // response) as a concurrent task and return immediately. That
                // keeps the event loop free: the request_permission
                // round-trip works, and other requests (session/cancel, _ri/*)
                // get served mid-turn.
                let s_prompt_task = Arc::clone(&s_prompt);
                let ctx_task = Arc::clone(&ctx);
                let session_id_task = session_id.clone();
                let emitter = V1Emitter::new(connection, responder);
                tokio_handle.spawn(async move {
                    run_turn(ctx_task, s_prompt_task, session_id_task, setup, emitter).await;
                });
                // Detach the JoinHandle: the turn task completes on its own.
                Ok(())
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
                drop(map);
                ctx_cancel.log(
                    "info",
                    Some(&notif.session_id.to_string()),
                    "session/cancel (hard abort)",
                );
                Ok(())
            },
            on_receive_notification!(),
        )
        // ── _ri/get_state (live mid-turn snapshot) ─────────────────────────
        .on_receive_request(
            async move |_req: RiGetStateRequest, responder, _connection| {
                responder.respond(ri_get_state(&ctx_state, &s_state))
            },
            on_receive_request!(),
        )
        // ── _ri/list_sessions (persisted, newest first) ──────────────────────
        .on_receive_request(
            async move |_req: RiListSessionsRequest, responder, _connection| {
                responder.respond(ri_list_sessions())
            },
            on_receive_request!(),
        )
        // ── _ri/steering (queue steering for the next prompt turn) ───────────
        .on_receive_request(
            async move |req: RiSteeringRequest, responder, _connection| {
                if !ctx_steer.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/steering");
                }
                responder.respond(ri_steering(
                    &s_steer,
                    &req.session_id,
                    &req.text,
                    &ctx_steer,
                ))
            },
            on_receive_request!(),
        )
        // ── _ri/delete_session ───────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiDeleteSessionRequest, responder, _connection| {
                if !ctx_del.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/delete_session");
                }
                responder.respond(ri_delete_session(&s_del, req.session_id.clone(), &ctx_del))
            },
            on_receive_request!(),
        )
        // ── _ri/prune_sessions (retention) ──────────────────────────────────
        .on_receive_request(
            async move |req: RiPruneSessionsRequest, responder, _connection| {
                if !ctx_prune.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/prune_sessions");
                }
                responder.respond(ri_prune_sessions(&ctx_prune, req.older_than_seconds))
            },
            on_receive_request!(),
        )
        // ── _ri/set_model ───────────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiSetModelRequest, responder, _connection| {
                if !ctx_set_model.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_model");
                }
                responder.respond(ri_set_model(&ctx_set_model, req.model))
            },
            on_receive_request!(),
        )
        // ── _ri/set_thinking ───────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiSetThinkingRequest, responder, _connection| {
                if !ctx_set_thinking.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_thinking");
                }
                responder.respond(ri_set_thinking(&ctx_set_thinking, req.level))
            },
            on_receive_request!(),
        )
        // ── _ri/set_provider (hot-swap the provider instance) ───────────────
        .on_receive_request(
            async move |req: RiSetProviderRequest, responder, _connection| {
                if !ctx_set_provider.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_provider");
                }
                responder.respond(ri_set_provider(&ctx_set_provider, req.provider))
            },
            on_receive_request!(),
        )
        // ── _ri/logs ────────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: RiLogsRequest, responder, _connection| {
                responder.respond(ri_logs(&ctx_logs, req.limit))
            },
            on_receive_request!(),
        )
}

// ── ACP protocol v2 (unstable) ───────────────────────────────────────────────

/// v2 replay mirror of `session_replay_updates`: maps committed session events
/// to v2 `UserMessageChunk`/`AgentMessageChunk` updates (per-turn `MessageId`).
fn session_replay_updates_v2(events: &[SessionEvent]) -> Vec<acp_v2::SessionUpdate> {
    let mut updates = Vec::new();
    let message_id = acp_v2::MessageId::new(format!("msg-{}", now_ts()));
    for ev in events {
        match ev {
            SessionEvent::UserMessage { content, .. } => updates.push(
                acp_v2::SessionUpdate::UserMessageChunk(acp_v2::ContentChunk::new(
                    acp_v2::ContentBlock::Text(acp_v2::TextContent::new(content.clone())),
                    message_id.clone(),
                )),
            ),
            SessionEvent::AssistantMessage { content, .. } => updates.push(
                acp_v2::SessionUpdate::AgentMessageChunk(acp_v2::ContentChunk::new(
                    acp_v2::ContentBlock::Text(acp_v2::TextContent::new(content.clone())),
                    message_id.clone(),
                )),
            ),
            _ => {}
        }
    }
    updates
}

/// v2 mirror of `synthesize_resource_reads`: embedded v2 text/blob resources in
/// a prompt generate `read_file` tool events so the agent sees their content.
fn synthesize_v2_resource_reads(blocks: &[acp_v2::ContentBlock]) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        let acp_v2::ContentBlock::Resource(r) = block else {
            continue;
        };
        let uri = match &r.resource {
            acp_v2::EmbeddedResourceResource::TextResourceContents(t) => t.uri.clone(),
            acp_v2::EmbeddedResourceResource::BlobResourceContents(b) => b.uri.clone(),
            _ => String::new(),
        };
        if uri.is_empty() {
            continue;
        }
        let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
        let ts = now_ts();
        let id = format!("attach_{idx}");
        out.push(SessionEvent::ToolCall {
            id: id.clone(),
            name: "read_file".to_string(),
            args: serde_json::json!({ "path": path }),
            include_in_llm: true,
            timestamp: ts,
        });
        if let acp_v2::EmbeddedResourceResource::TextResourceContents(t) = &r.resource {
            out.push(SessionEvent::ToolResult {
                id,
                name: "read_file".to_string(),
                content: t.text.clone(),
                is_error: false,
                display_range: None,
                include_in_llm: true,
                timestamp: ts,
            });
        }
    }
    out
}

/// Minimal v2 → user-text renderer. v2 clients typically send `Text` blocks;
/// resource/other blocks are preserved as a short placeholder (resource reads
/// are not yet synthesized for v2 prompts).
fn render_v2_prompt_blocks(blocks: &[acp_v2::ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            acp_v2::ContentBlock::Text(t) => out.push_str(&t.text),
            other => out.push_str(&format!("[{other:?}]")),
        }
    }
    out
}

/// v2 transport of the shared turn driver. Because v2's `session/update`
/// notifications attach a per-message `MessageId`, the emitter owns one for
/// the turn and threads it through every chunk.
struct V2Emitter {
    connection: ConnectionTo<Client>,
    responder: Responder<acp_v2::PromptResponse>,
    message_id: acp_v2::MessageId,
}

impl V2Emitter {
    fn new(connection: ConnectionTo<Client>, responder: Responder<acp_v2::PromptResponse>) -> Self {
        Self {
            connection,
            responder,
            message_id: acp_v2::MessageId::new(format!("msg-{}", now_ts())),
        }
    }

    fn send_update(&self, sid: &str, update: acp_v2::SessionUpdate) {
        let _ = self
            .connection
            .send_notification(acp_v2::UpdateSessionNotification::new(
                acp_v2::SessionId::new(sid.to_string()),
                update,
            ));
    }
}

impl TurnEmitter for V2Emitter {
    fn emit_agent_text(&self, sid: &str, text: &str) {
        self.send_update(
            sid,
            acp_v2::SessionUpdate::AgentMessageChunk(acp_v2::ContentChunk::new(
                acp_v2::ContentBlock::Text(acp_v2::TextContent::new(text)),
                self.message_id.clone(),
            )),
        );
    }

    fn emit_agent_thought(&self, sid: &str, text: &str) {
        self.send_update(
            sid,
            acp_v2::SessionUpdate::AgentThoughtChunk(acp_v2::ContentChunk::new(
                acp_v2::ContentBlock::Text(acp_v2::TextContent::new(text)),
                self.message_id.clone(),
            )),
        );
    }

    fn emit_usage(&self, sid: &str, u: &UsageStats, context_size: usize) {
        if let Some(used) = u.used_tokens() {
            self.send_update(
                sid,
                acp_v2::SessionUpdate::UsageUpdate(acp_v2::UsageUpdate::new(
                    (used as u64).min(context_size as u64),
                    context_size as u64,
                )),
            );
        }
    }

    fn emit_tool_pending(&self, sid: &str, id: &str, name: &str) {
        self.send_update(
            sid,
            acp_v2::SessionUpdate::ToolCallUpdate(
                acp_v2::ToolCallUpdate::new(acp_v2::ToolCallId::new(id.to_string()))
                    .title(name.to_string())
                    .kind(tool_kind_v2(name))
                    .status(acp_v2::ToolCallStatus::Pending),
            ),
        );
    }

    fn emit_tool_completed(&self, sid: &str, id: &str, content: &str) {
        self.send_update(
            sid,
            acp_v2::SessionUpdate::ToolCallUpdate(
                acp_v2::ToolCallUpdate::new(acp_v2::ToolCallId::new(id.to_string()))
                    .status(acp_v2::ToolCallStatus::Completed)
                    .content(vec![v2_tool_text(content)]),
            ),
        );
    }

    fn emit_tool_output_chunk(&self, sid: &str, id: &str, chunk: &str) {
        // Live tool output: stream each chunk as an in-progress tool-call
        // update; the final `Completed` update arrives on `ToolCallEnd`.
        self.send_update(
            sid,
            acp_v2::SessionUpdate::ToolCallUpdate(
                acp_v2::ToolCallUpdate::new(acp_v2::ToolCallId::new(id.to_string()))
                    .status(acp_v2::ToolCallStatus::InProgress)
                    .content(vec![v2_tool_text(chunk)]),
            ),
        );
    }

    async fn ask(
        &mut self,
        sid: &str,
        request: &AskRequest,
        cancel_rx: &mut tokio::sync::watch::Receiver<CancelLevel>,
    ) -> AskUserResponse {
        let rows = permission_option_rows(request);
        let mut title = request.question.clone();
        if let Some(ctx_text) = request.context.as_deref() {
            title = format!("{title}\n\n{ctx_text}");
        }
        let options: Vec<acp_v2::PermissionOption> = rows
            .iter()
            .map(|(id, t)| {
                acp_v2::PermissionOption::new(
                    acp_v2::PermissionOptionId::new(id.clone()),
                    t.clone(),
                    acp_v2::PermissionOptionKind::AllowOnce,
                )
            })
            .collect();
        let outcome = tokio::select! {
            o = self.connection.send_request(acp_v2::RequestPermissionRequest::new(
                acp_v2::SessionId::new(sid.to_string()),
                title,
                options,
            ))
            .block_task() => Some(o),
            _ = cancel_rx.wait_for(|l| *l >= CancelLevel::HardAbort) => None,
        };
        match outcome {
            Some(Ok(r)) => match &r.outcome {
                acp_v2::RequestPermissionOutcome::Selected(sel) => {
                    answer_from_selected(Some(sel.option_id.0.as_ref()), &rows)
                }
                _ => AskUserResponse::Cancelled,
            },
            _ => AskUserResponse::Cancelled,
        }
    }

    fn finish(self, _sid: &str, outcome: TurnOutcome) {
        let _ = match outcome.error {
            Some(msg) => self
                .responder
                .respond_with_error(agent_client_protocol::Error::internal_error().data(msg)),
            None => self.responder.respond(acp_v2::PromptResponse::new()),
        };
    }
}

/// Map a builtin tool name to the v2 `ToolKind`.
fn tool_kind_v2(name: &str) -> acp_v2::ToolKind {
    match name {
        "read_file" | "read_skill" => acp_v2::ToolKind::Read,
        "edit_file" | "write_file" => acp_v2::ToolKind::Edit,
        "find_files" => acp_v2::ToolKind::Search,
        "bash" | "exec" => acp_v2::ToolKind::Execute,
        "invoke_subagent" => acp_v2::ToolKind::Think,
        _ => acp_v2::ToolKind::Other,
    }
}

/// Wrap tool output text as a v2 tool-call content item.
fn v2_tool_text(text: &str) -> acp_v2::ToolCallContent {
    acp_v2::ToolCallContent::from(acp_v2::ContentBlock::Text(acp_v2::TextContent::new(text)))
}

/// Build the ACP protocol-v2 agent component (unstable): a bounded session
/// surface reusing the shared per-turn core. v2 coverage: `initialize`,
/// `session/new`, `session/prompt`, `session/cancel`, and `ask_user` →
/// `session/request_permission`. `_ri/*` custom methods and the v1-only
/// session conveniences (`session/load` etc.) are not served over v2 yet.
fn build_v2_agent(
    ctx: Arc<AcpContext>,
    sessions: Sessions,
) -> impl agent_client_protocol::ConnectTo<Client> {
    let s_new = Arc::clone(&sessions);
    let s_prompt = Arc::clone(&sessions);
    let s_resume = Arc::clone(&sessions);
    let s_list = Arc::clone(&sessions);
    let s_close = Arc::clone(&sessions);
    let s_fork = Arc::clone(&sessions);
    let s_delete = Arc::clone(&sessions);
    let s_cancel = Arc::clone(&sessions);
    let s_state = Arc::clone(&sessions);
    let s_steer = Arc::clone(&sessions);
    let s_del = Arc::clone(&sessions);
    let ctx_new = Arc::clone(&ctx);
    let ctx_resume = Arc::clone(&ctx);
    let ctx_close = Arc::clone(&ctx);
    let ctx_fork = Arc::clone(&ctx);
    let ctx_delete = Arc::clone(&ctx);
    let ctx_cancel = Arc::clone(&ctx);
    let ctx_state = Arc::clone(&ctx);
    let ctx_steer = Arc::clone(&ctx);
    let ctx_del = Arc::clone(&ctx);
    let ctx_prune = Arc::clone(&ctx);
    let ctx_set_model = Arc::clone(&ctx);
    let ctx_set_thinking = Arc::clone(&ctx);
    let ctx_set_provider = Arc::clone(&ctx);
    let ctx_logs = Arc::clone(&ctx);

    agent_client_protocol::Agent
        .v2()
        // ── initialize ─────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: acp_v2::InitializeRequest, responder, _connection| {
                let resp = acp_v2::InitializeResponse::new(
                    req.protocol_version,
                    acp_v2::Implementation::new("ri-agent", env!("CARGO_PKG_VERSION")),
                )
                .capabilities(
                    acp_v2::AgentCapabilities::new().session(acp_v2::SessionCapabilities::new()),
                );
                responder.respond(resp)
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/new ────────────────────────────────────────────────────
        .on_receive_request(
            async move |req: acp_v2::NewSessionRequest, responder, _connection| {
                let id = acp_v2::SessionId::new(format!("sess-{}", new_session_suffix()));
                // Store under the shared (v1-keyed) session map so both
                // protocol versions see the same sessions.
                let key = SessionId::new(id.0.as_ref().to_string());
                s_new
                    .lock()
                    .unwrap()
                    .insert(key, AcpSession::new(req.cwd.0.clone()));
                ctx_new.log(
                    "info",
                    Some(&id.to_string()),
                    format!("v2 session/new cwd={}", req.cwd.0.display()),
                );
                responder.respond(acp_v2::NewSessionResponse::new(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/prompt ─────────────────────────────────────────────────
        .on_receive_request(
            async move |req: acp_v2::PromptRequest, responder, connection| {
                let session_id = SessionId::new(req.session_id.0.as_ref().to_string());
                let prompt_text = render_v2_prompt_blocks(&req.prompt);
                ctx.log(
                    "info",
                    Some(&session_id.to_string()),
                    format!(
                        "v2 session/prompt start ({})",
                        prompt_text.chars().take(60).collect::<String>()
                    ),
                );
                let tokio_handle = ctx
                    .tokio_handle
                    .clone()
                    .unwrap_or_else(tokio::runtime::Handle::current);
                let setup = match prepare_turn(
                    &ctx,
                    &s_prompt,
                    &session_id,
                    prompt_text,
                    synthesize_v2_resource_reads(&req.prompt),
                )
                .await
                {
                    Ok(s) => s,
                    Err(msg) => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(msg),
                            )
                            .ok();
                        return Ok(());
                    }
                };
                // Same event-loop discipline as v1: run the turn on its own
                // task so the connection keeps reading client responses.
                let s_prompt_task = Arc::clone(&s_prompt);
                let ctx_task = Arc::clone(&ctx);
                let session_id_task = session_id.clone();
                let emitter = V2Emitter::new(connection, responder);
                tokio_handle.spawn(async move {
                    run_turn(ctx_task, s_prompt_task, session_id_task, setup, emitter).await;
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/resume (register + replay; shared map, v1-keyed) ───────
        .on_receive_request(
            async move |req: acp_v2::ResumeSessionRequest, responder, connection| {
                let key = SessionId::new(req.session_id.0.as_ref().to_string());
                let (events, _cwd) = match resolve_session_for_load(&s_resume, &key) {
                    SessionLoadResult::Ready { events, cwd } => (events, cwd),
                    SessionLoadResult::Busy => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("session is busy; cannot resume while a prompt runs"),
                            )
                            .ok();
                        return Ok(());
                    }
                    SessionLoadResult::Unknown => {
                        responder
                            .respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data("unknown session"),
                            )
                            .ok();
                        return Ok(());
                    }
                };
                ctx_resume.log("info", Some(&key.to_string()), "v2 session/resume");
                for u in session_replay_updates_v2(&events) {
                    let _ = connection.send_notification(acp_v2::UpdateSessionNotification::new(
                        req.session_id.clone(),
                        u,
                    ));
                }
                responder.respond(acp_v2::ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/list (persisted first, then live in-memory) ────────────
        .on_receive_request(
            async move |_req: acp_v2::ListSessionsRequest, responder, _connection| {
                let mut sessions: Vec<acp_v2::SessionInfo> = Vec::new();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in persisted_session_list() {
                    seen.insert(p.id.clone());
                    sessions.push(acp_v2::SessionInfo::new(
                        acp_v2::SessionId::new(p.id),
                        acp_v2::AbsolutePath::new(p.cwd),
                    ));
                }
                {
                    let map = s_list.lock().unwrap();
                    let mut live: Vec<(String, std::path::PathBuf)> = map
                        .iter()
                        .filter(|(id, _)| !seen.contains(&id.to_string()))
                        .map(|(id, s)| (id.to_string(), s.cwd.clone()))
                        .collect();
                    live.sort();
                    for (id, cwd) in live {
                        sessions.push(acp_v2::SessionInfo::new(
                            acp_v2::SessionId::new(id),
                            acp_v2::AbsolutePath::new(cwd),
                        ));
                    }
                }
                responder.respond(acp_v2::ListSessionsResponse::new(sessions))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/close (drop in-memory; history stays resumable) ─────────
        .on_receive_request(
            async move |req: acp_v2::CloseSessionRequest, responder, _connection| {
                let key = SessionId::new(req.session_id.0.as_ref().to_string());
                let mut map = s_close.lock().unwrap();
                if let Some(s) = map.get(&key)
                    && s.running
                {
                    drop(map);
                    responder
                        .respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("session is busy; cannot close while a prompt runs"),
                        )
                        .ok();
                    return Ok(());
                }
                let removed = map.remove(&key).is_some();
                drop(map);
                ctx_close.log(
                    "info",
                    Some(&key.to_string()),
                    if removed {
                        "v2 session/close".to_string()
                    } else {
                        "v2 session/close (already closed)".to_string()
                    },
                );
                responder.respond(acp_v2::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/fork (clone live or persisted session into a new id) ───
        .on_receive_request(
            async move |req: acp_v2::ForkSessionRequest, responder, _connection| {
                let key = SessionId::new(req.session_id.0.as_ref().to_string());
                let src = {
                    let map = s_fork.lock().unwrap();
                    map.get(&key).map(|s| (s.events.clone(), s.cwd.clone()))
                };
                let (events, cwd) = match src {
                    Some(x) => x,
                    None => match read_persisted_session(&key) {
                        Some(p) => (p.events, p.cwd),
                        None => {
                            responder
                                .respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data("unknown session"),
                                )
                                .ok();
                            return Ok(());
                        }
                    },
                };

                let new_id = acp_v2::SessionId::new(format!("sess-{}", new_session_suffix()));
                let new_key = SessionId::new(new_id.0.as_ref().to_string());
                let mut session = AcpSession::new(cwd.clone());
                session.events = events.clone();
                {
                    let mut map = s_fork.lock().unwrap();
                    map.insert(new_key.clone(), session);
                }
                persist_session(&new_key, &events, &cwd);
                ctx_fork.log(
                    "info",
                    Some(&key.to_string()),
                    format!("v2 session/fork -> {new_id} ({} events)", events.len()),
                );
                responder.respond(acp_v2::ForkSessionResponse::new(new_id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/delete (in-memory + persisted file) ─────────────────────
        .on_receive_request(
            async move |req: acp_v2::DeleteSessionRequest, responder, _connection| {
                let key = SessionId::new(req.session_id.0.as_ref().to_string());
                let mut error: Option<String> = None;
                let mut deleted_in_memory = false;
                {
                    let mut map = s_delete.lock().unwrap();
                    if let Some(s) = map.get(&key) {
                        if s.running {
                            error =
                                Some("session is busy; cancel the active prompt first".to_string());
                        } else {
                            map.remove(&key);
                            deleted_in_memory = true;
                        }
                    }
                }
                let deleted_on_disk = delete_persisted_session(&key);
                if let Some(e) = error {
                    responder
                        .respond_with_error(agent_client_protocol::Error::invalid_params().data(e))
                        .ok();
                    return Ok(());
                }
                ctx_delete.log(
                    "info",
                    Some(&key.to_string()),
                    format!(
                        "v2 session/delete (memory={deleted_in_memory}, disk={deleted_on_disk})"
                    ),
                );
                responder.respond(acp_v2::DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── ri-specific `_ri/*` (version-neutral, shared implementations) ────
        .on_receive_request(
            async move |_req: RiGetStateRequest, responder, _connection| {
                responder.respond(ri_get_state(&ctx_state, &s_state))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: RiListSessionsRequest, responder, _connection| {
                responder.respond(ri_list_sessions())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiSteeringRequest, responder, _connection| {
                if !ctx_steer.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/steering");
                }
                responder.respond(ri_steering(
                    &s_steer,
                    &req.session_id,
                    &req.text,
                    &ctx_steer,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiDeleteSessionRequest, responder, _connection| {
                if !ctx_del.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/delete_session");
                }
                responder.respond(ri_delete_session(&s_del, req.session_id.clone(), &ctx_del))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiPruneSessionsRequest, responder, _connection| {
                if !ctx_prune.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/prune_sessions");
                }
                responder.respond(ri_prune_sessions(&ctx_prune, req.older_than_seconds))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiSetModelRequest, responder, _connection| {
                if !ctx_set_model.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_model");
                }
                responder.respond(ri_set_model(&ctx_set_model, req.model))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiSetThinkingRequest, responder, _connection| {
                if !ctx_set_thinking.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_thinking");
                }
                responder.respond(ri_set_thinking(&ctx_set_thinking, req.level))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiSetProviderRequest, responder, _connection| {
                if !ctx_set_provider.authorize(req.token.as_deref()) {
                    unauthorized!(responder, "_ri/set_provider");
                }
                responder.respond(ri_set_provider(&ctx_set_provider, req.provider))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: RiLogsRequest, responder, _connection| {
                responder.respond(ri_logs(&ctx_logs, req.limit))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/cancel ─────────────────────────────────────────────────
        .on_receive_notification(
            async move |notif: acp_v2::CancelSessionNotification, _connection| {
                let key = SessionId::new(notif.session_id.0.as_ref().to_string());
                let mut map = s_cancel.lock().unwrap();
                if let Some(s) = map.get_mut(&key)
                    && let Some(tx) = s.cancel_tx.as_ref()
                {
                    let _ = tx.send(CancelLevel::HardAbort);
                }
                drop(map);
                ctx_cancel.log(
                    "info",
                    Some(&notif.session_id.to_string()),
                    "v2 session/cancel (hard abort)",
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Build the agent component, negotiating protocol v1 or v2 from each
/// connection's `initialize`. v1 serves the full surface; v2 serves the
/// bounded v2 surface reusing the same per-turn core.
fn build_agent_router(
    ctx: Arc<AcpContext>,
    sessions: Sessions,
) -> impl agent_client_protocol::ConnectTo<Client> {
    agent_client_protocol::Agent
        .protocol_router()
        .with_v1(build_agent(Arc::clone(&ctx), sessions.clone()))
        .with_v2(build_v2_agent(ctx, sessions))
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
    build_agent_router(ctx, sessions)
        .connect_to(Stdio::new())
        .await
}

/// Run the ACP over HTTP + WebSocket on `addr` (axum).
pub async fn run_acp_ws(
    ctx: Arc<AcpContext>,
    addr: std::net::SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
) -> anyhow::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let factory = {
        let ctx = Arc::clone(&ctx);
        let sessions = sessions.clone();
        move || build_agent_router(ctx.clone(), sessions.clone())
    };
    let server = agent_client_protocol_http::AcpHttpServer::new(factory);
    let router = server.into_router();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    match tls {
        Some((cert, key)) => {
            let acceptor = build_tls_acceptor(&cert, &key)?;
            axum::serve(
                TlsServer {
                    inner: listener,
                    acceptor,
                },
                router,
            )
            .await?;
        }
        None => axum::serve(listener, router).await?,
    }
    Ok(())
}

/// Minimal axum [`axum::serve::Listener`] that upgrades each accepted TCP
/// connection to TLS. Connections that fail the handshake are logged and
/// dropped rather than propagated as errors.
struct TlsServer {
    inner: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsServer {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (tcp, addr) = match self.inner.accept().await {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("ri ACP: accept error: {e}");
                    continue;
                }
            };
            match self.acceptor.accept(tcp).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    eprintln!("ri ACP: TLS handshake failed from {addr}: {e}");
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Build a `TlsAcceptor` from PEM cert + key files (PKCS#8 or PKCS#1).
fn build_tls_acceptor(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> = {
        let data = fs::read(cert_path)?;
        let mut reader = std::io::Cursor::new(data);
        rustls_pemfile::certs(&mut reader).collect::<Result<_, _>>()?
    };
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", cert_path.display());
    }
    let key: PrivateKeyDer<'static> = {
        let data = fs::read(key_path)?;
        let mut reader = std::io::Cursor::new(data);
        rustls_pemfile::private_key(&mut reader)?
            .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path.display()))?
    };
    // Install the crate's default crypto provider (already built for musl via
    // the HTTP stack). Ignore "already installed" errors.
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(anyhow::Error::msg)?;
    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)))
}

/// Result of resolving a session's history for `session/load` / `session/resume`.
enum SessionLoadResult {
    Ready {
        events: Vec<SessionEvent>,
        cwd: PathBuf,
    },
    Busy,
    Unknown,
}

/// Live in-memory session first (must not be running), else a persisted one
/// (registered in memory so subsequent prompts continue from it).
fn resolve_session_for_load(sessions: &Sessions, id: &SessionId) -> SessionLoadResult {
    let in_memory = {
        let map = sessions.lock().unwrap();
        map.get(id)
            .map(|s| (s.running, s.events.clone(), s.cwd.clone()))
    };
    match in_memory {
        Some((true, _, _)) => SessionLoadResult::Busy,
        Some((false, events, cwd)) => SessionLoadResult::Ready { events, cwd },
        None => {
            let Some(persisted) = read_persisted_session(id) else {
                return SessionLoadResult::Unknown;
            };
            let mut map = sessions.lock().unwrap();
            let slot = map
                .entry(id.clone())
                .or_insert_with(|| AcpSession::new(persisted.cwd.clone()));
            slot.events = persisted.events.clone();
            SessionLoadResult::Ready {
                events: persisted.events.clone(),
                cwd: persisted.cwd,
            }
        }
    }
}

/// Unix seconds → RFC3339 UTC (Howard Hinnant civil-from-days algorithm).
fn unix_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
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
fn permission_option(id: String, title: &str) -> PermissionOption {
    PermissionOption::new(id, title.to_string(), PermissionOptionKind::AllowOnce)
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
    fn synthesizes_v2_resource_reads_as_read_file_events() {
        use agent_client_protocol::schema::v2::{
            ContentBlock, EmbeddedResource, EmbeddedResourceResource, TextResourceContents,
        };
        let blocks = vec![
            ContentBlock::Text(acp_v2::TextContent::new("hi")),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "print(1)",
                    "file:///abs/v2.py",
                )),
            )),
        ];
        let events = synthesize_v2_resource_reads(&blocks);
        assert_eq!(events.len(), 2, "one read_file ToolCall + ToolResult");
        assert!(matches!(
            &events[0],
            SessionEvent::ToolCall { name, args, .. }
                if name == "read_file" && args["path"] == "/abs/v2.py"
        ));
        assert!(matches!(
            &events[1],
            SessionEvent::ToolResult { content, .. } if content == "print(1)"
        ));
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
    fn permission_option_rows_map_titles_descriptions_and_continue_escape() {
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
        let rows = permission_option_rows(&req);
        assert_eq!(rows.len(), 3, "freeform ask: 2 options + Continue");
        assert_eq!(rows[0], ("opt-0".to_string(), "Yes".to_string()));
        assert_eq!(rows[1].1, "No");
        assert_eq!(rows[2], ("continue".to_string(), "Continue".to_string()));

        assert!(matches!(
            answer_from_selected(Some("opt-0"), &rows),
            AskUserResponse::Answer(a) if a == "Yes"
        ));
        assert!(matches!(
            answer_from_selected(Some("opt-9"), &rows),
            AskUserResponse::Answer(a) if a == "opt-9"
        ));
        assert!(matches!(
            answer_from_selected(None, &rows),
            AskUserResponse::Cancelled
        ));

        // Non-freeform asks with options: no Continue injection.
        let (req, _) = ask_request_opts(
            vec![AskUserOption {
                title: "Run tests".to_string(),
                description: Some("cargo test --all-features".to_string()),
            }],
            false,
            false,
        );
        let rows = permission_option_rows(&req);
        assert_eq!(rows.len(), 1, "non-freeform ask: no Continue escape");
        assert_eq!(
            rows[0].1, "Run tests — cargo test --all-features",
            "description folded into visible label"
        );

        // Empty options degenerate to a Continue escape in all cases.
        let (bare, _) = ask_request_opts(vec![], false, false);
        let rows = permission_option_rows(&bare);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("continue".to_string(), "Continue".to_string()));

        // version-specific mapping of rows -> v1 PermissionOption list.
        let opts: Vec<PermissionOption> = rows
            .iter()
            .map(|(id, t)| permission_option(id.clone(), t))
            .collect();
        assert_eq!(opts[0].option_id.0.as_ref(), "continue");
        assert_eq!(opts[0].name, "Continue");
    }
    #[test]
    fn answer_from_selected_maps_titles_and_cancelled() {
        let (req, _) = ask_request(vec![AskUserOption {
            title: "Run tests".to_string(),
            description: None,
        }]);
        let rows = permission_option_rows(&req);

        assert!(matches!(
            answer_from_selected(Some("opt-0"), &rows),
            AskUserResponse::Answer(t) if t == "Run tests"
        ));
        // No selection (cancelled / errored transport) -> Cancelled.
        assert!(matches!(
            answer_from_selected(None, &rows),
            AskUserResponse::Cancelled
        ));
    }

    #[test]
    fn answer_from_selected_falls_back_to_option_id_for_unknown_selection() {
        let rows = [("opt-0".to_string(), "Run tests".to_string())];
        assert!(matches!(
            answer_from_selected(Some("mystery"), &rows),
            AskUserResponse::Answer(t) if t == "mystery"
        ));
    }

    // ── disk persistence round-trip ──────────────────────────────────────────

    #[test]
    fn request_permission_response_serializes_as_expected() {
        use agent_client_protocol::schema::v1::{
            PermissionOptionId as Id, RequestPermissionOutcome as O,
            RequestPermissionResponse as R, SelectedPermissionOutcome,
        };
        // RequestPermissionOutcome is INTERNALLY tagged: Selected -> {"outcome":"selected",...}.
        let selected = R::new(O::Selected(SelectedPermissionOutcome::new(Id::new(
            "opt-0",
        ))));
        let v = serde_json::to_value(&selected).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"outcome":{"outcome":"selected","optionId":"opt-0"}})
        );

        let cancelled = R::new(O::Cancelled);
        let v2 = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(v2, serde_json::json!({"outcome":{"outcome":"cancelled"}}));
    }

    #[test]
    fn unix_to_rfc3339_formats_utc() {
        // 2024-04-13T09:20:00Z = 1713000000
        assert_eq!(unix_to_rfc3339(1_713_000_000), "2024-04-13T09:20:00Z");
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1), "1970-01-01T00:00:01Z");
        assert_eq!(unix_to_rfc3339(1_021_200_600), "2002-05-12T10:50:00Z");
    }

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

#[cfg(test)]
mod acp_e2e {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{
        CancelNotification, CloseSessionRequest, ContentBlock, InitializeRequest,
        NewSessionRequest, PermissionOptionId, PromptRequest, RequestPermissionOutcome,
        RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
        SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
        TextContent,
    };
    use agent_client_protocol::{
        Agent as AcpRoleAgent, Client, ConnectionTo, on_receive_notification, on_receive_request,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    fn test_ctx() -> Arc<AcpContext> {
        let provider: Arc<dyn LlmProvider + Send + Sync + 'static> =
            Arc::new(crate::llm::test_provider::TestProvider::new());
        let rebuild: ProviderRebuild = Arc::new(|_, _, _| {
            Ok((
                Arc::new(crate::llm::test_provider::TestProvider::new())
                    as Arc<dyn LlmProvider + Send + Sync>,
                "test".to_string(),
            ))
        });
        Arc::new(AcpContext {
            provider: RwLock::new(provider),
            model: RwLock::new("test".to_string()),
            thinking: RwLock::new(ThinkingLevel::High),
            rebuild,
            tokio_handle: Some(tokio::runtime::Handle::current()),
            file_tracker: Arc::new(Mutex::new(FileTracker::new())),
            skills: Arc::new(Vec::new()),
            logs: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            admin_token: None,
        })
    }

    fn sessions() -> Sessions {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(s))
    }

    /// Best-effort cleanup of the disk-persisted session file this test created.
    fn cleanup_session(id: &SessionId) {
        if let Some(dir) = acp_sessions_dir() {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_echo_turn_streams_text() {
        let ctx = test_ctx();
        let s = sessions();
        let agent = build_agent(ctx, s);
        let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let chunks2 = Arc::clone(&chunks);
        let client = Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    if let SessionUpdate::AgentMessageChunk(c) = n.update
                        && let ContentBlock::Text(t) = c.content
                    {
                        chunks2.lock().unwrap().push(t.text.to_string());
                    }
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V1);
                assert!(init.agent_capabilities.load_session);
                let ns = cx
                    .send_request(NewSessionRequest::new(
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
                    ))
                    .block_task()
                    .await?;
                let sid = ns.session_id;
                let pr = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![text_block("echo merhaba-acp-inproc")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(pr.stop_reason, StopReason::EndTurn);
                cleanup_session(&sid);
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("e2e timed out")
            .expect("client connection failed");
        let text = chunks.lock().unwrap().join("");
        assert!(
            text.contains("merhaba-acp-inproc"),
            "echo text missing; got {text:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_ask_user_round_trip() {
        let ctx = test_ctx();
        let s = sessions();
        let agent = build_agent(ctx, s.clone());
        let perm_titles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let perm_titles2 = Arc::clone(&perm_titles);
        let client = Client
            .builder()
            .on_receive_request(
                async move |r: RequestPermissionRequest, responder, _cx| {
                    for o in &r.options {
                        perm_titles2.lock().unwrap().push(o.name.clone());
                    }
                    let option_id = r
                        .options
                        .first()
                        .map(|o| o.option_id.clone())
                        .unwrap_or_else(|| PermissionOptionId::new("continue"));
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        )),
                    ))
                },
                on_receive_request!(),
            )
            .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                let _ = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let ns = cx
                    .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                    .block_task()
                    .await?;
                let sid = ns.session_id;
                let pr = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![text_block(
                            "tool ask_user {\"question\":\"devam edeyim mi?\",\"options\":[\"evet\",\"hayir\"]}",
                        )],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(pr.stop_reason, StopReason::EndTurn);

                // The chosen answer must have been routed back into the agent
                // and committed into the in-memory session history.
                let ev = { s.lock().unwrap().get(&sid).map(|x| x.events.clone()) };
                assert!(
                    ev.is_some_and(|events| events.iter().any(|e| matches!(
                        e,
                        SessionEvent::ToolResult { content, .. } if content.contains("evet")
                    ))),
                    "ask answer not recorded in session history"
                );
                cleanup_session(&sid);
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("ask e2e timed out")
            .expect("client connection failed");
        let opts = perm_titles.lock().unwrap().clone();
        assert_eq!(
            opts.as_slice(),
            &["evet", "hayir", "Continue"],
            "request_permission options differ: {opts:?}"
        );
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_midturn_getstate_reports_streaming() {
        let ctx = test_ctx();
        let agent = build_agent(ctx, sessions());
        let client =
            Client
                .builder()
                .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                    let _ = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let ns = cx
                        .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                        .block_task()
                        .await?;
                    let sid = ns.session_id;

                    // Query get_state from a concurrent task while the prompt
                    // streams, then assert it reported the live streaming session.
                    let cx2 = cx.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    cx2.spawn({
                        let cx_in = cx2.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            let r = cx_in.send_request(RiGetStateRequest).block_task().await;
                            let _ = tx.send(r.map(|x| x.streaming_sessions).unwrap_or(0));
                            Ok(())
                        }
                    })?;

                    let words = (0..30)
                        .map(|i| format!("w{i}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let pr = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![text_block(&format!("slow {words}"))],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(pr.stop_reason, StopReason::EndTurn);
                    let streaming = rx.await.unwrap_or(0);
                    assert!(
                        streaming > 0,
                        "mid-turn _ri/get_state should report a streaming session"
                    );
                    cleanup_session(&sid);
                    Ok(())
                });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("mid-turn e2e timed out")
            .expect("client connection failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_sessions_list_close_resume() {
        let ctx = test_ctx();
        let agent = build_agent(ctx, sessions());
        let replay: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let replay2 = Arc::clone(&replay);
        let client = Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    if let SessionUpdate::AgentMessageChunk(c) = n.update
                        && let ContentBlock::Text(t) = c.content
                    {
                        replay2.lock().unwrap().push_str(&t.text);
                    }
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                let _ = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let ns = cx
                    .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                    .block_task()
                    .await?;
                let sid = ns.session_id;
                let _ = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![text_block("echo otokrat-marker")],
                    ))
                    .block_task()
                    .await?;

                // The finished session is persisted and listed.
                let list = cx.send_request(RiListSessionsRequest).block_task().await?;
                assert!(
                    list.sessions.iter().any(|m| m.id == sid.to_string()),
                    "persisted session not listed in _ri/list_sessions"
                );

                // Close drops the in-memory session (idempotent), then resume
                // re-registers it from disk and replay streams the old turn.
                let _ = cx
                    .send_request(CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                let _ = cx
                    .send_request(CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                let _ = cx
                    .send_request(ResumeSessionRequest::new(
                        sid.clone(),
                        std::path::PathBuf::from("/tmp"),
                    ))
                    .block_task()
                    .await?;
                // The resumed session is usable again.
                let pr = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![text_block("echo after-resume")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(pr.stop_reason, StopReason::EndTurn);
                cleanup_session(&sid);
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("sessions e2e timed out")
            .expect("client connection failed");
        assert!(
            replay.lock().unwrap().contains("otokrat-marker"),
            "resume did not replay the earlier turn: {:?}",
            replay.lock().unwrap()
        );
        assert!(
            replay.lock().unwrap().contains("after-resume"),
            "resumed session did not stream its follow-up turn: {:?}",
            replay.lock().unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_cancel_resolves_pending_ask() {
        let ctx = test_ctx();
        let agent = build_agent(ctx, sessions());
        let client =
            Client
                .builder()
                .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                    let _ = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let ns = cx
                        .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                        .block_task()
                        .await?;
                    let sid = ns.session_id;

                    // Fire a session/cancel while the ask is still pending.
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let cx2 = cx.clone();
                    let sid2 = sid.clone();
                    cx2.spawn({
                        let cx_in = cx2.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            let _ = cx_in.send_notification(CancelNotification::new(sid2.clone()));
                            let _ = tx.send(());
                            Ok(())
                        }
                    })?;

                    // The ask prompt completes (end_turn, not a hang) even though
                    // no permission answer was ever sent.
                    let pr = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![text_block(
                                "tool ask_user {\"question\":\"cevap?\",\"options\":[\"evet\"]}",
                            )],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(pr.stop_reason, StopReason::EndTurn);
                    rx.await.expect("cancel task did not finish");

                    // The session is still usable after the cancelled ask.
                    let pr2 = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![text_block("echo bitti")],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(pr2.stop_reason, StopReason::EndTurn);
                    cleanup_session(&sid);
                    Ok(())
                });

        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("cancel e2e timed out")
            .expect("client connection failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_set_provider_hot_swap_and_set_model() {
        let ctx = test_ctx();
        let agent = build_agent(ctx, sessions());
        let client =
            Client
                .builder()
                .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                    let _ = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let ns = cx
                        .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                        .block_task()
                        .await?;
                    let sid = ns.session_id;

                    let pr = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![text_block("echo birinci")],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(pr.stop_reason, StopReason::EndTurn);

                    // Hot-swap the provider instance by id (test provider), then a
                    // model change on the newly-selected instance.
                    let sp = cx
                        .send_request(RiSetProviderRequest {
                            provider: "test".to_string(),
                            token: None,
                        })
                        .block_task()
                        .await?;
                    assert!(sp.ok, "set_provider failed: {:?}", sp.error);
                    assert_eq!(sp.model, "test");
                    let sm = cx
                        .send_request(RiSetModelRequest {
                            model: "swapped-model".to_string(),
                            token: None,
                        })
                        .block_task()
                        .await?;
                    assert!(sm.ok, "set_model after swap failed: {:?}", sm.error);
                    assert_eq!(sm.model, "swapped-model");

                    // A subsequent prompt still works against the swapped provider.
                    let pr2 = cx
                        .send_request(PromptRequest::new(
                            sid.clone(),
                            vec![text_block("echo ikinci")],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(pr2.stop_reason, StopReason::EndTurn);
                    cleanup_session(&sid);
                    Ok(())
                });

        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("set_provider e2e timed out")
            .expect("client connection failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acp_inproc_protocol_v2_prompt_echo_negotiates_v2() {
        let ctx = test_ctx();
        let agent = build_agent_router(ctx, sessions());
        let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let chunks2 = Arc::clone(&chunks);
        let client = agent_client_protocol::Client
            .v2()
            .on_receive_notification(
                async move |n: acp_v2::UpdateSessionNotification, _cx| {
                    if let acp_v2::SessionUpdate::AgentMessageChunk(c) = &n.update
                        && let acp_v2::ContentBlock::Text(t) = &c.content
                    {
                        chunks2.lock().unwrap().push(t.text.to_string());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |r: acp_v2::RequestPermissionRequest, responder, _cx| {
                    let option_id = r
                        .options
                        .first()
                        .map(|o| o.option_id.clone())
                        .unwrap_or_else(|| acp_v2::PermissionOptionId::new("continue"));
                    responder.respond(acp_v2::RequestPermissionResponse::new(
                        acp_v2::RequestPermissionOutcome::Selected(
                            acp_v2::SelectedPermissionOutcome::new(option_id),
                        ),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |cx: ConnectionTo<AcpRoleAgent>| async move {
                // Negotiate protocol v2 explicitly.
                let init = cx
                    .send_request(acp_v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        acp_v2::Implementation::new("ri-e2e-v2", "1.0"),
                    ))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V2);

                let ns = cx
                    .send_request(acp_v2::NewSessionRequest::new(acp_v2::AbsolutePath::new(
                        "/tmp",
                    )))
                    .block_task()
                    .await?;
                let sid = ns.session_id;

                let pr = cx
                    .send_request(acp_v2::PromptRequest::new(
                        sid.clone(),
                        vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                            "echo v2-merhaba-inproc",
                        ))],
                    ))
                    .block_task()
                    .await?;
                // v2 PromptResponse is minimal (no stop_reason); its presence
                // means the turn completed through the v2 surface.
                let _ = &pr;

                // Ask over v2: the server emits a v2 request_permission; our
                // registered handler auto-selects the first option.
                let pr2 = cx
                    .send_request(acp_v2::PromptRequest::new(
                        sid.clone(),
                        vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                            "tool ask_user {\"question\":\"v2 onay?\",\"options\":[\"evet\"]}",
                        ))],
                    ))
                    .block_task()
                    .await?;
                let _ = &pr2;

                // Cancel over v2: fires a CancelSessionNotification; the turn
                // machinery (already covered by v1) resolves any pending ask.
                let _ = cx.send_notification(acp_v2::CancelSessionNotification::new(sid.clone()));

                // Standard v2 session lifecycle over the shared map.
                let list = cx
                    .send_request(acp_v2::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert!(
                    list.sessions.iter().any(|s| s.session_id == sid),
                    "v2 session/list missing the session"
                );

                let forked = cx
                    .send_request(acp_v2::ForkSessionRequest::new(
                        sid.clone(),
                        acp_v2::AbsolutePath::new("/tmp"),
                    ))
                    .block_task()
                    .await?;
                let fork_id = forked.session_id;

                let resume = cx
                    .send_request(acp_v2::ResumeSessionRequest::new(
                        fork_id.clone(),
                        acp_v2::AbsolutePath::new("/tmp"),
                    ))
                    .block_task()
                    .await?;
                let _ = &resume;

                // close drops the in-memory session; the persisted file remains
                let _ = cx
                    .send_request(acp_v2::CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                // delete removes the persisted file
                let _ = cx
                    .send_request(acp_v2::DeleteSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;

                cleanup_v1_session(&fork_id);
                cleanup_v1_session(&sid);

                // ri-specific `_ri/*` methods are also served over v2 (shared
                // implementations registered on both agents).
                let st = cx.send_request(RiGetStateRequest).block_task().await?;
                assert_eq!(st.model, "test");

                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("v2 e2e timed out")
            .expect("client connection failed");
        let text = chunks.lock().unwrap().join("");
        assert!(
            text.contains("v2-merhaba-inproc"),
            "v2 streamed text missing; got {text:?}"
        );
    }

    /// Remove the shared persistence file for a v2 session id (wrapped into the
    /// shared v1-keyed map).
    fn cleanup_v1_session(sid: &acp_v2::SessionId) {
        cleanup_session(&SessionId::new(sid.0.as_ref().to_string()));
    }
}
