//! `invoke_subagent` — run a named subagent and capture its final answer.
//!
//! Bridges the orchestrator to a subagent definition (`mode: subagent` in an
//! agent root). The heavy lifting lives in [`crate::agent::subagent::run_subagent`];
//! this tool only parses its arguments and resolves the [`SubagentContext`]
//! provided by the executor.

use std::pin::Pin;

use serde_json::Value;

use crate::agent::subagent::run_subagent;
use crate::agent::types::{Tool, ToolCallContext, ToolResult};

pub struct InvokeSubagentTool;

#[derive(serde::Deserialize)]
struct InvokeSubagentArgs {
    name: String,
    task: String,
}

impl Tool for InvokeSubagentTool {
    fn name(&self) -> &str {
        "invoke_subagent"
    }

    fn description(&self) -> &str {
        "Run a named subagent (an agent profile with `mode: subagent`) on a focused \\\n         task and return its final answer. The subagent runs with its own system \\\n         prompt, tool limitations, and a bounded number of steps; its live output \\\n         is streamed under this call. Use when a subtask benefits from a dedicated \\\n         system prompt or a restricted tool set."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the subagent to invoke (must be defined with `mode: subagent` under an agent root)"
                },
                "task": {
                    "type": "string",
                    "description": "The focused task/instructions to hand to the subagent"
                }
            },
            "required": ["name", "task"]
        })
    }

    fn streaming_field(&self) -> Option<&'static str> {
        Some("task")
    }

    fn run(
        &self,
        args: Value,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let InvokeSubagentArgs { name, task } = match super::parse_args(args) {
                Ok(a) => a,
                Err(e) => return *e,
            };

            let Some(sub) = ctx.subagent else {
                return ToolResult::err(
                    "invoke_subagent is not available in this context \
                     (subagent launching is not wired up).",
                );
            };

            run_subagent(&sub, &name, &task, &ctx.id, ctx.tx, ctx.cancel_rx).await
        })
    }
}
