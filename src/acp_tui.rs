//! ACP **client** bridge that lets the ratatui TUI drive a *detached* agent.
//!
//! `ri --tui-acp` keeps the interactive TUI, but replaces its in-process
//! [`run_agent_loop`] with a child `ri --serve` subprocess speaking the Agent
//! Client Protocol over stdio. The TUI then becomes a pure ACP client:
//!
//! - each submitted user message is sent as `session/prompt`,
//! - `session/update` notifications are translated back into [`AgentEvent`]s
//!   and pushed onto the same `AppEvent` channel the local loop would use, so
//!   the TUI rendering, history, and step-back screens are untouched,
//! - `session/request_permission` is surfaced through the TUI ask dialog
//!   (`AppEvent::AskUser`) and answered with the picked option,
//! - user cancel presses (`Ctrl-C`) map to `session/cancel`.
//!
//! This is the decoupled "TUI on ACP" mode: the UI and the agent are two
//! processes communicating exclusively over the protocol. Default (`Local`)
//! behaviour is unchanged and remains the in-process route.

use std::process::Stdio;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionUpdate, ToolCallContent,
    ToolCallStatus, ToolCallUpdate as AcpToolCallUpdate,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, watch};

use crate::agent::types::{
    AgentEvent, AskRequest, AskUserOption, AskUserResponse, CancelLevel, ToolResult,
};
use crate::app_event::{AppEvent, AppEventTx, SendIgnore};
use crate::llm::{AssistantPhase, UsageStats};

/// Control handles handed back to the TUI after a successful spawn.
pub struct AcpTuiControls {
    /// Send the text of a submitted user message (one prompt at a time).
    pub prompt_tx: UnboundedSender<String>,
    /// Cancel channel the TUI presses to abort the active prompt.
    pub cancel_tx: watch::Sender<CancelLevel>,
    /// The orchestration task (aborted on drop so the child is reaped).
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AcpTuiControls {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn a child `ri --serve` (this executable, forwarding `child_args` such
/// as `--provider test`) and connect an ACP session to it. The returned task
/// pumps child stdout forever; drop/abort it to stop.
pub async fn spawn(
    child_args: Vec<String>,
    cwd: String,
    app_event_tx: AppEventTx,
) -> anyhow::Result<AcpTuiControls> {
    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--serve").args(&child_args).current_dir(&cwd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout unavailable"))?;
    let mut lines = BufReader::new(stdout).lines();

    // ── handshake: initialize + session/new (no notifications interleave) ──
    write_line(
        &mut stdin,
        json!({
            "jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{
                "protocolVersion":1,
                "clientCapabilities":{},
                "clientInfo":{"name":"ri-tui-acp","title":"ri TUI (ACP)","version":"1"}
            }
        }),
    )
    .await?;
    read_response(&mut lines, 0).await?;

    write_line(
        &mut stdin,
        json!({
            "jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd": cwd, "mcpServers":[]}
        }),
    )
    .await?;
    let new_resp = read_response(&mut lines, 1).await?;
    let session_id: String = new_resp
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        anyhow::bail!("session/new returned no sessionId: {new_resp}");
    }

    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (cancel_tx, cancel_rx) = watch::channel(CancelLevel::None);

    let task = tokio::spawn(run_loop(
        child,
        stdin,
        lines,
        session_id,
        prompt_rx,
        cancel_rx,
        app_event_tx,
    ));

    Ok(AcpTuiControls {
        prompt_tx,
        cancel_tx,
        task,
    })
}

/// Write one JSON-RPC message as a newline-terminated line.
async fn write_line(stdin: &mut tokio::process::ChildStdin, value: Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(&value)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

/// Read stdout lines until a JSON-RPC response with the given id arrives.
async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: i64,
) -> anyhow::Result<Value> {
    loop {
        let Some(line) = lines.next_line().await? else {
            anyhow::bail!("ACP server closed stdout during setup");
        };
        let value: Value = serde_json::from_str(&line)?;
        if value.get("id").and_then(|v| v.as_i64()) == Some(expected_id) {
            if let Some(err) = value.get("error") {
                anyhow::bail!("ACP setup request {expected_id} failed: {err}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// RAII guard that kills the child process on drop (task abort or EOF).
struct ChildGuard(tokio::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    mut lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    session_id: String,
    mut prompt_rx: UnboundedReceiver<String>,
    mut cancel_rx: watch::Receiver<CancelLevel>,
    app_event_tx: AppEventTx,
) {
    let mut child = ChildGuard(child);
    let mut next_id: i64 = 10;
    let mut prompt_pending = false;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Err(e) => {
                        eprintln!("ri ACP client: read error: {e}");
                        app_event_tx.send_ignore(AppEvent::Agent(AgentEvent::Error(
                            crate::llm::ProviderError::other("acp_tui", format!("ACP client read error: {e}")),
                        )));
                        let _ = child.0.start_kill();
                        break;
                    }
                    Ok(None) => {
                        // Server exited.
                        if prompt_pending {
                            app_event_tx.send_ignore(AppEvent::Agent(AgentEvent::Error(
                                crate::llm::ProviderError::other(
                                    "acp_tui",
                                    "ACP server exited during the turn",
                                ),
                            )));
                        }
                        let _ = child.0.start_kill();
                        break;
                    }
                    Ok(Some(line)) => {
                        let value: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let method = value.get("method").and_then(|m| m.as_str()).map(str::to_string);
                        let has_request = value.get("id").is_some() && method.is_some();
                        let has_response = value.get("id").is_some() && method.is_none();

                        if has_request {
                            // Reverse-direction request (session/request_permission).
                            handle_server_request(&value, &mut stdin, &mut next_id, &app_event_tx)
                                .await;
                        } else if has_response {
                            // session/prompt completion.
                            prompt_pending = false;
                            if let Some(err) = value.get("error") {
                                app_event_tx.send_ignore(AppEvent::Agent(AgentEvent::Error(
                                    crate::llm::ProviderError::other(
                                        "acp_tui",
                                        serde_json::to_string(err).unwrap_or_default(),
                                    ),
                                )));
                            } else {
                                end_turn(&app_event_tx);
                            }
                        } else if method.as_deref() == Some("session/update") {
                            handle_update(&value, &app_event_tx);
                        }
                    }
                }
            }
            Some(text) = prompt_rx.recv() => {
                if prompt_pending {
                    continue; // one prompt at a time
                }
                let req = json!({
                    "jsonrpc":"2.0","id": next_id, "method":"session/prompt",
                    "params": {"sessionId": session_id, "prompt": [
                        {"type":"text","text": text}
                    ]}
                });
                next_id += 1;
                if write_line(&mut stdin, req).await.is_ok() {
                    prompt_pending = true;
                }
            }
            changed = cancel_rx.changed() => {
                if changed.is_ok() && *cancel_rx.borrow() >= CancelLevel::HardAbort && prompt_pending {
                    let _ = write_line(
                        &mut stdin,
                        json!({
                            "jsonrpc":"2.0","method":"session/cancel",
                            "params":{"sessionId": session_id}
                        }),
                    )
                    .await;
                    prompt_pending = false;
                    // The server aborts the prompt; surface a clean end so the
                    // TUI turn wraps up.
                    end_turn(&app_event_tx);
                }
            }
        }
    }
}

/// Translate one `session/update` notification into agent events.
fn handle_update(value: &Value, tx: &AppEventTx) {
    let Ok(update) = serde_json::from_value::<SessionUpdate>(value["params"]["update"].clone())
    else {
        return;
    };
    for ev in translate_update(update) {
        tx.send_ignore(AppEvent::Agent(ev));
    }
}

/// A `session/request_permission` request from the server: surface it through
/// the TUI ask dialog and respond once the user picks an option (or cancels).
async fn handle_server_request(
    value: &Value,
    stdin: &mut tokio::process::ChildStdin,
    next_id: &mut i64,
    tx: &AppEventTx,
) {
    let Some(id) = value.get("id").and_then(|v| v.as_i64()) else {
        return;
    };
    let _ = next_id;
    let params = &value["params"];
    let req: RequestPermissionRequest = match serde_json::from_value(params.clone()) {
        Ok(r) => r,
        Err(_) => return,
    };

    let options: Vec<AskUserOption> = req
        .options
        .iter()
        .map(|o| AskUserOption {
            title: o.name.clone(),
            description: None,
        })
        .collect();
    let question = req
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "The agent is requesting permission.".to_string());
    let (reply, reply_rx) = oneshot::channel();
    tx.send_ignore(AppEvent::AskUser(AskRequest {
        question,
        context: None,
        options,
        allow_multiple: false,
        allow_freeform: true,
        reply,
    }));

    let answer = reply_rx.await.ok();
    let response = match answer {
        Some(AskUserResponse::Answer(title)) => {
            let picked = req
                .options
                .iter()
                .find(|o| o.name == title)
                .map(|o| o.option_id.0.to_string())
                .unwrap_or_else(|| "continue".to_string());
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(picked),
            ))
        }
        _ => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    };
    let result = serde_json::to_value(response).unwrap_or(Value::Null);
    let _ = write_line(stdin, json!({"jsonrpc":"2.0","id": id, "result": result})).await;
}

/// Emit the same terminal events the local loop emits at a turn boundary.
fn end_turn(tx: &AppEventTx) {
    tx.send_ignore(AppEvent::Agent(AgentEvent::TurnEnd));
    tx.send_ignore(AppEvent::Agent(AgentEvent::Done));
}

/// Map an ACP `SessionUpdate` onto the ri `AgentEvent` vocabulary.
fn translate_update(update: SessionUpdate) -> Vec<AgentEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => text_chunk(&chunk)
            .map(|text| AgentEvent::TextToken {
                text,
                phase: AssistantPhase::Final,
            })
            .into_iter()
            .collect(),
        SessionUpdate::AgentThoughtChunk(chunk) => text_chunk(&chunk)
            .map(AgentEvent::ThinkingToken)
            .into_iter()
            .collect(),
        SessionUpdate::ToolCall(call) => {
            let name = call.title.clone();
            vec![
                AgentEvent::ToolCallIntent {
                    id: call.tool_call_id.to_string(),
                    name: name.clone(),
                    streaming_field: None,
                },
                AgentEvent::ToolCallStart {
                    id: call.tool_call_id.to_string(),
                    name,
                    args: json!({}),
                },
            ]
        }
        SessionUpdate::ToolCallUpdate(update) => tool_update_events(update),
        SessionUpdate::UsageUpdate(u) => {
            let used = u.used.min(u.size);
            vec![AgentEvent::Usage(UsageStats {
                input_tokens: None,
                output_tokens: Some(used as usize),
                total_tokens: Some(used as usize),
                cached_tokens: None,
                reasoning_tokens: None,
            })]
        }
        _ => Vec::new(),
    }
}

fn text_chunk(chunk: &ContentChunk) -> Option<String> {
    match &chunk.content {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// `tool_call_update` → live in-progress output or the final completed result.
fn tool_update_events(update: AcpToolCallUpdate) -> Vec<AgentEvent> {
    let text = update
        .fields
        .content
        .unwrap_or_default()
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<String>();
    let id = update.tool_call_id.to_string();
    match update.fields.status {
        Some(ToolCallStatus::InProgress) | Some(ToolCallStatus::Pending) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![AgentEvent::ToolOutputChunk { id, chunk: text }]
            }
        }
        Some(ToolCallStatus::Completed) => vec![AgentEvent::ToolCallEnd {
            id,
            result: ToolResult::ok_str(if text.is_empty() {
                "(no output)".to_string()
            } else {
                text
            }),
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(s),
        ))
    }

    #[test]
    fn translates_text_thought_usage_and_tool_events() {
        use agent_client_protocol::schema::v1::{
            ToolCall as AcpToolCall, ToolCallUpdateFields, ToolKind, UsageUpdate,
        };

        let evs = translate_update(SessionUpdate::AgentMessageChunk(text_block("merhaba")));
        assert!(
            matches!(&evs[..], [AgentEvent::TextToken { text, phase }] if text == "merhaba" && *phase == AssistantPhase::Final)
        );

        let evs = translate_update(SessionUpdate::AgentThoughtChunk(text_block("hmm")));
        assert!(matches!(&evs[..], [AgentEvent::ThinkingToken(t)] if t == "hmm"));

        let tool = SessionUpdate::ToolCall(
            AcpToolCall::new("t1", "bash")
                .kind(ToolKind::Execute)
                .status(agent_client_protocol::schema::v1::ToolCallStatus::Pending),
        );
        let evs = translate_update(tool);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], AgentEvent::ToolCallIntent { name, .. } if name == "bash"));
        assert!(matches!(&evs[1], AgentEvent::ToolCallStart { name, .. } if name == "bash"));

        let progress = SessionUpdate::ToolCallUpdate(AcpToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("line-1\n"),
                ))]),
        ));
        let evs = translate_update(progress);
        assert!(
            matches!(&evs[..], [AgentEvent::ToolOutputChunk { id, chunk }] if id == "t1" && chunk == "line-1\n")
        );

        let done = SessionUpdate::ToolCallUpdate(AcpToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("out"),
                ))]),
        ));
        let evs = translate_update(done);
        assert!(matches!(&evs[..], [AgentEvent::ToolCallEnd { id, .. }] if id == "t1"));

        let usage = SessionUpdate::UsageUpdate(UsageUpdate::new(42, 1000));
        let evs = translate_update(usage);
        assert!(matches!(
            &evs[..],
            [AgentEvent::Usage(u)] if u.total_tokens == Some(42)
        ));
    }

    #[test]
    fn tool_result_ok_str_preserves_content() {
        let r = ToolResult::ok_str("hello");
        assert!(!r.is_error);
        assert_eq!(r.content.as_text(), "hello");
    }
}
