//! A hidden test provider for exercising the ri-agent UI without a real API
//! connection.  Activated via `--provider=test` or `/provider test`.
//! Never appears in the provider selection menu.  Never persists to config.
//!
//! Kept intentionally minimal: it exists to smoke-test the agent loop and,
//! crucially, to let you inspect the exact system prompt being sent to the
//! model via the `system` command.  The heavier scripted demo sequences were
//! trimmed.

use async_stream::stream;
use tokio::time::{Duration, sleep};

use super::{
    AssistantPhase, LlmEvent, LlmRequestContext, LlmStream, Message, ModelListFuture, Role,
    ToolDefinition,
};
use crate::llm::ProviderError;

/// A hidden test provider with no persistent state.
pub struct TestProvider;

impl TestProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TestProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ── Help text ─────────────────────────────────────────────────────────────────

const HELP_TEXT: &str = r#"# Test Provider Commands

| Command         | Description                                                     |
|-----------------|-----------------------------------------------------------------|
| `help`          | Show this help                                                  |
| `echo <text>`   | Stream text back to the UI                                      |
| `slow <text>`   | Stream text with artificial per-word delays                     |
| `system`        | Show the exact system prompt that is being sent to the model    |
| `error`         | Emit a provider error to exercise the error-display path        |
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Stream a static string as word-sized tokens with an optional per-word delay.
fn stream_text(text: &'static str, delay: Option<Duration>) -> LlmStream {
    Box::pin(stream! {
        for word in text.split_inclusive(' ') {
            if let Some(d) = delay {
                sleep(d).await;
            }
            yield LlmEvent::Token {
                text: word.to_string(),
                phase: AssistantPhase::Final,
            };
        }
        yield LlmEvent::Done;
    })
}

/// Stream an owned string as word-sized tokens.
fn stream_owned(text: String) -> LlmStream {
    Box::pin(stream! {
        for word in text.split_inclusive(' ').map(ToOwned::to_owned).collect::<Vec<_>>() {
            yield LlmEvent::Token {
                text: word,
                phase: AssistantPhase::Final,
            };
        }
        yield LlmEvent::Done;
    })
}

// ── LlmProvider impl ──────────────────────────────────────────────────────────

impl super::LlmProvider for TestProvider {
    fn stream_chat(&self, messages: Vec<Message>, context: LlmRequestContext) -> LlmStream {
        self.stream_chat_with_tools(messages, vec![], context)
    }

    fn stream_chat_with_tools(
        &self,
        messages: Vec<Message>,
        _tools: Vec<ToolDefinition>,
        _context: LlmRequestContext,
    ) -> LlmStream {
        // If the last message is a tool result, echo it back so the provider
        // keeps working in a multi-turn agent loop.
        if let Some(last) = messages.last()
            && last.role == Role::ToolResult
        {
            let content = last.content.clone();
            let response = format!("Tool result:\n\n```\n{content}\n```\n");
            return stream_owned(response);
        }

        // Otherwise parse the last user message as a command.
        let input = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.trim().to_string())
            .unwrap_or_default();

        let (cmd, rest) = match input.split_once(char::is_whitespace) {
            Some((c, r)) => (c.to_ascii_lowercase(), r.trim().to_string()),
            None => (input.to_ascii_lowercase(), String::new()),
        };

        match cmd.as_str() {
            "help" => stream_text(HELP_TEXT, None),

            "echo" => {
                let text = if rest.is_empty() {
                    "(nothing to echo)".to_string()
                } else {
                    rest
                };
                stream_owned(text + "\n")
            }

            "slow" => {
                let text = if rest.is_empty() {
                    "(nothing to slow-stream)".to_string()
                } else {
                    rest
                };
                Box::pin(stream! {
                    for word in text.split_inclusive(' ').map(ToOwned::to_owned).collect::<Vec<_>>() {
                        sleep(Duration::from_millis(150)).await;
                        yield LlmEvent::Token {
                            text: word,
                            phase: AssistantPhase::Final,
                        };
                    }
                    yield LlmEvent::Done;
                })
            }

            "system" => {
                // The key capability: surface the exact system prompt that is
                // being sent to the model, verbatim, in a fenced code block.
                let system_content = messages
                    .iter()
                    .find(|m| m.role == Role::System)
                    .map(|m| m.content.clone())
                    .unwrap_or_else(|| "(no system prompt found)".to_string());
                let response = format!("System prompt:\n\n```\n{system_content}\n```\n");
                stream_owned(response)
            }

            "error" => Box::pin(stream! {
                yield LlmEvent::Error(ProviderError::other("test", "test error triggered by 'error' command"));
            }),

            "" => stream_text("Type 'help' for a list of test provider commands.\n", None),

            _ => {
                let msg = format!("Unknown command: '{cmd}'. Type 'help' for a list.\n");
                stream_owned(msg)
            }
        }
    }

    fn list_models(&self) -> ModelListFuture {
        Box::pin(async { Ok(vec!["test".to_string()]) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmProvider;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn system_command_returns_exact_system_prompt() {
        let provider = TestProvider::new();
        let events: Vec<LlmEvent> = provider
            .stream_chat(
                vec![Message::system("YOU ARE RI."), Message::user("system")],
                LlmRequestContext::default(),
            )
            .collect()
            .await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        assert!(text.contains("System prompt:"), "{text}");
        assert!(
            text.contains("YOU ARE RI."),
            "system prompt not echoed: {text}"
        );
    }

    #[tokio::test]
    async fn unknown_command_returns_hint() {
        let provider = TestProvider::new();
        let events: Vec<LlmEvent> = provider
            .stream_chat(
                vec![Message::user("does-not-exist")],
                LlmRequestContext::default(),
            )
            .collect()
            .await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        assert!(text.contains("Unknown command"), "{text}");
    }
}
