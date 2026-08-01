//! Subagent invocation support for the `invoke_subagent` tool.
//!
//! A subagent is a named [`AgentMeta`] with `mode: subagent` living under an
//! agent root (`~/.ri/agents/<name>/SYSTEM.md` or a project-local root).
//! When the orchestrator calls `invoke_subagent`, the runner:
//!
//! 1. resolves the agent by name;
//! 2. builds a filtered tool/skill set from the agent's include/exclude rules,
//!    minus `invoke_subagent` itself (which prevents unbounded recursion);
//! 3. runs a bounded nested loop with its own system prompt, forwarding live
//!    output chunks under the outer tool call so the UI shows it working;
//! 4. returns the subagent's final text as the tool result.
//!
//! Subagent transcripts are ephemeral — they only flow into the subagent's own
//! LLM context and are never persisted to the session event log. Only the
//! final answer becomes the outer `ToolCall`/`ToolResult` event pair.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch::Receiver;

use crate::agent::ToolOutputLog;
use crate::agent::build_sorted_tool_defs;
use crate::agent::system_prompt::build_system_prompt;
use crate::agent::types::{
    AgentEvent, CancelLevel, SubagentContext, ToolCallContext, ToolRegistry, ToolResult,
};
use crate::agents::{AgentMeta, AgentMode, filter_skills, filter_tools, resolve_agent};
use crate::app_event::AppEvent;
use crate::llm::{LlmEvent, LlmProvider, Message, ToolDefinition};

/// Maximum number of LLM turns a subagent may consume before it is force-stopped.
const MAX_SUBAGENT_TURNS: usize = 20;

/// Tool the subagent must never expose to itself (prevents unbounded recursion).
const SUBAGENT_TOOL_NAME: &str = "invoke_subagent";

/// Build the tool registry available to a subagent from the outer universe.
fn build_subagent_registry(outer: &ToolRegistry, agent: &AgentMeta) -> ToolRegistry {
    let mut filtered = filter_tools(outer, &agent.include_tools, &agent.exclude_tools);
    // A subagent can never invoke another subagent, regardless of filters.
    filtered.remove(SUBAGENT_TOOL_NAME);
    filtered
}

/// Build the system prompt for a subagent.
fn build_subagent_system_prompt(
    tools: &ToolRegistry,
    skills: &[crate::skills::SkillMeta],
    agent: &AgentMeta,
    cwd: &str,
) -> String {
    let filtered_skills = filter_skills(skills, &agent.include_skills, &agent.exclude_skills);
    let mut prompt = build_system_prompt(tools, cwd, &filtered_skills, Some(agent));
    // Surface the definition location so relative references inside SYSTEM.md
    // / AGENTS.md can be resolved against the right directory.
    prompt.push_str(&format!(
        "\n\nThis agent's instructions were loaded from `{}` (base directory: `{}`); \
         resolve relative paths in your instructions against that base directory.",
        agent.path.display(),
        agent.base_dir.display(),
    ));
    prompt
}

/// Outcome of one subagent streaming turn.
enum SubTurnOutcome {
    FinalAnswer {
        text: String,
    },
    ToolCalls {
        text: String,
        calls: Vec<(String, String, serde_json::Value)>,
    },
    ToolIntentWithNoCall,
    Error(crate::llm::ProviderError),
}

/// Stream one subagent turn, forwarding text/thinking as live tool-output
/// chunks under `outer_id` so the orchestrator's UI shows the work in progress.
///
/// Unlike the top-level loop, subagent tokens are NOT emitted as
/// `TextToken`/`ThinkingToken` events — doing so would corrupt the outer
/// transcript. They surface only as tool output belonging to the
/// `invoke_subagent` call.
async fn stream_subagent_turn(
    provider: std::sync::Arc<dyn LlmProvider + Send + Sync + 'static>,
    messages: Vec<Message>,
    tool_defs: Vec<ToolDefinition>,
    outer_id: &str,
    tx: &UnboundedSender<AppEvent>,
) -> SubTurnOutcome {
    let mut stream = provider.stream(messages, tool_defs);

    let mut assistant_text = String::new();
    let mut pending_calls = Vec::new();
    let mut intent_seen = false;
    let mut chunk = String::new();

    while let Some(ev) = stream.next().await {
        match ev {
            LlmEvent::Token { text, .. } => {
                assistant_text.push_str(&text);
                chunk.push_str(&text);
                if chunk.len() >= 64 {
                    let _ = tx.send(AppEvent::Agent(AgentEvent::ToolOutputChunk {
                        id: outer_id.to_string(),
                        chunk: std::mem::take(&mut chunk),
                    }));
                }
            }
            LlmEvent::ThinkingToken(t) => chunk.push_str(&t),
            LlmEvent::ToolCallStart { .. } => intent_seen = true,
            LlmEvent::ToolCall { id, name, args } => pending_calls.push((id, name, args)),
            LlmEvent::ToolCallArgsDelta { .. } => {}
            LlmEvent::Usage(_) => {}
            LlmEvent::Done => break,
            LlmEvent::Error(e) => return SubTurnOutcome::Error(e),
        }
    }

    if !chunk.is_empty() {
        let _ = tx.send(AppEvent::Agent(AgentEvent::ToolOutputChunk {
            id: outer_id.to_string(),
            chunk,
        }));
    }

    if intent_seen && pending_calls.is_empty() {
        return SubTurnOutcome::ToolIntentWithNoCall;
    }

    if pending_calls.is_empty() {
        SubTurnOutcome::FinalAnswer {
            text: assistant_text,
        }
    } else {
        SubTurnOutcome::ToolCalls {
            text: assistant_text,
            calls: pending_calls,
        }
    }
}

/// Run a named subagent to completion and return its final answer as a
/// [`ToolResult`].
///
/// `tx` / `cancel_rx` may be `None` (tests, non-streaming runs); the runner
/// creates benign fallbacks so it never needs a live UI.
pub async fn run_subagent(
    sub: &SubagentContext,
    name: &str,
    task: &str,
    outer_id: &str,
    tx: Option<UnboundedSender<AppEvent>>,
    cancel_rx: Option<Receiver<CancelLevel>>,
) -> ToolResult {
    let tx = tx.unwrap_or_else(|| {
        let (t, _r) = tokio::sync::mpsc::unbounded_channel();
        t
    });
    let cancel_rx = cancel_rx.unwrap_or_else(|| {
        let (_t, r) = tokio::sync::watch::channel(CancelLevel::None);
        r
    });

    let Some(agent) = resolve_agent(&sub.agents, name) else {
        let known = sub
            .agents
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return ToolResult::err(format!(
            "invoke_subagent: no agent named `{name}`. Known agents: {known}"
        ));
    };
    if agent.mode != AgentMode::Subagent {
        return ToolResult::err(format!(
            "invoke_subagent: agent `{name}` is a primary agent, not a subagent. \
             Set `mode: subagent` in its SYSTEM.md to make it invokable."
        ));
    }

    let tools = build_subagent_registry(&sub.tools, agent);
    let system_prompt = build_subagent_system_prompt(&tools, &sub.skills, agent, &sub.cwd);
    let mut messages = vec![
        Message::system(system_prompt),
        Message::user(task.to_string()),
    ];
    let mut log = ToolOutputLog::new(&format!("subagent-{name}"));

    for turn in 1..=MAX_SUBAGENT_TURNS {
        if *cancel_rx.borrow() >= CancelLevel::HardAbort {
            return ToolResult::err("subagent cancelled by user");
        }

        let tool_defs = build_sorted_tool_defs(&tools);
        match stream_subagent_turn(
            Arc::clone(&sub.provider),
            messages.clone(),
            tool_defs,
            outer_id,
            &tx,
        )
        .await
        {
            SubTurnOutcome::FinalAnswer { text } => {
                let text = text.trim();
                if text.is_empty() {
                    return ToolResult::ok_str(format!(
                        "[subagent `{name}` finished without producing output after \
                         {turn} turn(s)]"
                    ));
                }
                return ToolResult::ok_str(text.to_string());
            }
            SubTurnOutcome::ToolCalls { text, calls } => {
                messages.push(Message::assistant(text));
                for (id, tool_name, args) in &calls {
                    let tool_ctx = ToolCallContext {
                        id: id.clone(),
                        tx: Some(tx.clone()),
                        cancel_rx: Some(cancel_rx.clone()),
                        // No nested subagents: the outer cancellation channel and
                        // registry are reused, but invoke_subagent was stripped.
                        subagent: None,
                    };

                    let result = match tools.get(tool_name) {
                        Some(tool) => tool.run(args.clone(), tool_ctx).await,
                        None => ToolResult::err(format!(
                            "subagent tried to call unknown tool `{tool_name}`"
                        )),
                    };
                    let cmd_summary = args.get("command").and_then(|v| v.as_str());
                    let result = result.with_log_notice(id, cmd_summary, &mut log);

                    messages.push(Message::tool_call(
                        id.clone(),
                        tool_name.clone(),
                        args.clone(),
                    ));
                    messages.push(Message::tool_result(
                        id.clone(),
                        result.content.as_text().to_string(),
                        result.is_error,
                    ));
                }
            }
            SubTurnOutcome::ToolIntentWithNoCall => {
                return ToolResult::err(format!(
                    "subagent `{name}` indicated a tool call that did not arrive \
                     (response may have been truncated)."
                ));
            }
            SubTurnOutcome::Error(e) => {
                return ToolResult::err(format!("subagent `{name}` failed: {}", e.message));
            }
        }
    }

    ToolResult::err(format!(
        "subagent `{name}` did not finish within {MAX_SUBAGENT_TURNS} turns."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{Tool, ToolRegistry};
    use crate::agents::AgentMeta;

    fn test_agent(mode: AgentMode) -> AgentMeta {
        AgentMeta {
            name: "helper".to_string(),
            description: "a test subagent".to_string(),
            mode,
            include_tools: vec!["read_*".to_string()],
            exclude_tools: vec![],
            include_skills: vec![],
            exclude_skills: vec![],
            system_prompt: "You are a focused helper.".to_string(),
            agents_md: None,
            path: std::path::PathBuf::from("/tmp/agents/helper/SYSTEM.md"),
            base_dir: std::path::PathBuf::from("/tmp/agents/helper"),
        }
    }

    fn stub_registry(names: &[&str]) -> ToolRegistry {
        struct Stub(String);
        impl Tool for Stub {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                "stub"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn run(
                &self,
                _args: serde_json::Value,
                _ctx: ToolCallContext,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                Box::pin(async { ToolResult::ok_str("stub") })
            }
        }
        let mut r = ToolRegistry::new();
        for n in names {
            r.insert(
                (*n).to_string(),
                Arc::new(Stub((*n).to_string())) as Arc<dyn Tool>,
            );
        }
        r
    }

    #[test]
    fn subagent_registry_respects_filters_and_strips_invoke_subagent() {
        let outer = stub_registry(&[
            "invoke_subagent",
            "read_file",
            "read_skill",
            "bash",
            "ask_user",
        ]);
        let agent = test_agent(AgentMode::Subagent);
        let registry = build_subagent_registry(&outer, &agent);

        // Filter `read_*` keeps read_file/read_skill, drops bash; always-present
        // ask_user survives; invoke_subagent is always stripped.
        assert!(registry.contains_key("read_file"));
        assert!(registry.contains_key("read_skill"));
        assert!(registry.contains_key("ask_user"));
        assert!(!registry.contains_key("bash"));
        assert!(!registry.contains_key("invoke_subagent"));
    }

    #[test]
    fn subagent_system_prompt_mentions_definition_path() {
        let registry = stub_registry(&["read_file"]);
        let agent = test_agent(AgentMode::Subagent);
        let prompt = build_subagent_system_prompt(&registry, &[], &agent, "/tmp");
        assert!(prompt.contains("You are a focused helper."));
        assert!(prompt.contains("/tmp/agents/helper/SYSTEM.md"));
        assert!(prompt.contains("/tmp/agents/helper"));
    }

    #[tokio::test]
    async fn run_subagent_errors_on_unknown_or_primary_agent() {
        let sub = SubagentContext {
            provider: Arc::new(crate::llm::test_provider::TestProvider),
            agents: Arc::new(vec![test_agent(AgentMode::Primary)]),
            skills: Arc::new(vec![]),
            cwd: "/tmp".to_string(),
            tools: stub_registry(&[]),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let unknown = run_subagent(&sub, "nope", "task", "outer", Some(tx.clone()), None).await;
        assert!(unknown.is_error);
        assert!(unknown.content.as_text().contains("no agent named `nope`"));

        let primary = run_subagent(&sub, "helper", "task", "outer", Some(tx), None).await;
        assert!(primary.is_error);
        assert!(primary.content.as_text().contains("not a subagent"));
    }

    #[tokio::test]
    async fn run_subagent_executes_tools_and_returns_final_answer() {
        use crate::agent::types::AgentEvent;
        use crate::app_event::AppEvent;
        use crate::llm::{AssistantPhase, LlmStream, ModelListFuture, Role};
        use futures_util::stream;

        // History-aware mock: before a tool result, request a `bash` call;
        // after the tool result has been fed back, emit the final answer.
        struct ScriptProvider;
        impl LlmProvider for ScriptProvider {
            fn stream(&self, messages: Vec<Message>, _tools: Vec<ToolDefinition>) -> LlmStream {
                let has_tool_result = messages.iter().any(|m| m.role == Role::ToolResult);
                if has_tool_result {
                    Box::pin(stream::iter(vec![
                        LlmEvent::Token {
                            text: "subagent done".to_string(),
                            phase: AssistantPhase::Unknown,
                        },
                        LlmEvent::Done,
                    ]))
                } else {
                    Box::pin(stream::iter(vec![
                        LlmEvent::ToolCall {
                            id: "sub_call_1".to_string(),
                            name: "bash".to_string(),
                            args: serde_json::json!({ "command": "echo hi" }),
                        },
                        LlmEvent::Done,
                    ]))
                }
            }
            fn list_models(&self) -> ModelListFuture {
                Box::pin(async { Ok(vec![]) })
            }
        }

        let mut agent = test_agent(AgentMode::Subagent);
        agent.include_tools = vec!["*".to_string()];
        let sub = SubagentContext {
            provider: Arc::new(ScriptProvider),
            agents: Arc::new(vec![agent]),
            skills: Arc::new(vec![]),
            cwd: "/tmp".to_string(),
            tools: stub_registry(&["bash", "invoke_subagent"]),
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = run_subagent(&sub, "helper", "do the thing", "outer_1", Some(tx), None).await;

        assert!(
            !result.is_error,
            "subagent should succeed: {:?}",
            result.content.as_text()
        );
        assert_eq!(result.content.as_text(), "subagent done");

        // Live output was streamed under the outer tool id (stub tool + text).
        let mut saw_chunk = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Agent(AgentEvent::ToolOutputChunk { id, .. }) = ev {
                if id == "outer_1" {
                    saw_chunk = true;
                }
            }
        }
        assert!(saw_chunk, "expected live ToolOutputChunk under outer_1");
    }
}
