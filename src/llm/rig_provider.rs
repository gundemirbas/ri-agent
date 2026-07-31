//! OpenAI-compatible provider backed by the `rig-core` crate.
//!
//! Adapter that maps xs-agent's [`Message`]/[`ToolDefinition`]/[`LlmEvent`]
//! types onto rig's completion request + raw streaming response, preserving
//! live observation of text tokens, thinking (reasoning) tokens, and tool-call
//! argument deltas.  It targets any OpenAI-compatible endpoint (OpenAI API,
//! DeepSeek/vLLM, Open WebUI, ollama's OpenAI endpoint, …) via a custom
//! `base_url`.

use async_stream::stream;
use futures_util::StreamExt;
use rig_core::OneOrMany;
use rig_core::client::CompletionClient;
use rig_core::completion::request::Usage as RigUsage;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, GetTokenUsage,
    ToolDefinition as RigToolDefinition,
};
use rig_core::message as rig_message;
use rig_core::message::Message as RigMessage;
use rig_core::providers::openai::CompletionsClient;
use rig_core::streaming::{StreamedAssistantContent, ToolCallDeltaContent};

use super::{
    AssistantPhase, LlmEvent, LlmRequestContext, LlmStream, Message, ModelListFuture, Role,
    ToolDefinition, UsageStats,
};
use crate::llm::{ProviderError, ProviderErrorKind};

/// OpenAI-compatible provider built on rig.
pub struct RigOpenAiProvider {
    client: CompletionsClient,
    model: String,
}

impl RigOpenAiProvider {
    /// Create a provider for an OpenAI-compatible endpoint.
    ///
    /// `base_url` should include the API version prefix, e.g.
    /// `http://localhost:8000/v1` (rig appends `/chat/completions`).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Self::new_with_headers(base_url, model, api_key, vec![])
    }

    /// Create a provider with extra per-request HTTP headers (e.g. OpenRouter's
    /// `HTTP-Referer` / `X-Title`).
    pub fn new_with_headers(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let mut builder = CompletionsClient::builder().api_key(api_key.into());
        builder = builder.base_url(&base_url);
        if !headers.is_empty() {
            let mut map = rig_core::http_client::HeaderMap::new();
            for (k, v) in headers {
                let (Ok(k), Ok(v)) = (
                    http::HeaderName::from_bytes(k.as_bytes()),
                    http::HeaderValue::from_str(&v),
                ) else {
                    continue;
                };
                map.insert(k, v);
            }
            builder = builder.http_headers(map);
        }
        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build OpenAI-compatible client: {e}"))?;
        Ok(Self {
            client,
            model: model.into(),
        })
    }
}

// ── Message conversion ────────────────────────────────────────────────────────

/// Convert a ri-agent [`Message`] history to rig's typed messages.
///
/// Mirrors the grouping used by the OpenAI wire format: an assistant message
/// is merged with its immediately following tool-call messages into a single
/// assistant message carrying both text and `tool_calls`; the corresponding
/// tool results are emitted afterwards as user messages.
pub(crate) fn to_rig_messages(messages: &[Message]) -> Vec<RigMessage> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            Role::System => {
                out.push(RigMessage::system(msg.content.clone()));
                i += 1;
            }
            Role::User => {
                out.push(RigMessage::user(msg.content.clone()));
                i += 1;
            }
            Role::Assistant => {
                // Collect this assistant message plus consecutive ToolCall
                // messages into one rig assistant message.
                let mut content: Vec<rig_message::AssistantContent> = Vec::new();
                if !msg.content.is_empty() {
                    content.push(rig_message::AssistantContent::text(msg.content.clone()));
                }
                if let Some(thinking) = msg.thinking.as_deref().filter(|t| !t.is_empty()) {
                    content.push(rig_message::AssistantContent::reasoning(thinking));
                }
                let mut j = i + 1;
                while j < messages.len() && messages[j].role == Role::ToolCall {
                    let tc = &messages[j];
                    content.push(rig_message::AssistantContent::tool_call(
                        tc.tool_call_id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", out.len())),
                        tc.tool_name.clone().unwrap_or_default(),
                        tc.tool_args.clone().unwrap_or(serde_json::json!({})),
                    ));
                    j += 1;
                }
                out.push(RigMessage::Assistant {
                    id: None,
                    content: OneOrMany::from_iter_optional(content)
                        .unwrap_or_else(|| OneOrMany::one(rig_message::AssistantContent::text(""))),
                });
                i = j;
            }
            Role::ToolCall => {
                // Standalone tool call (no preceding assistant text) — emit as
                // its own assistant message so the result pairing is preserved.
                out.push(RigMessage::Assistant {
                    id: None,
                    content: OneOrMany::one(rig_message::AssistantContent::tool_call(
                        msg.tool_call_id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", out.len())),
                        msg.tool_name.clone().unwrap_or_default(),
                        msg.tool_args.clone().unwrap_or(serde_json::json!({})),
                    )),
                });
                i += 1;
            }
            Role::ToolResult => {
                let id = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", out.len()));
                out.push(RigMessage::tool_result(id, msg.content.clone()));
                i += 1;
            }
        }
    }
    out
}

/// Convert ri-agent [`ToolDefinition`]s to rig's typed tool definitions.
pub(crate) fn to_rig_tools(tools: &[ToolDefinition]) -> Vec<RigToolDefinition> {
    tools
        .iter()
        .map(|t| RigToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect()
}

/// Map rig token usage onto ri-agent [`UsageStats`].
fn to_usage_stats(u: &RigUsage) -> UsageStats {
    UsageStats {
        input_tokens: (u.input_tokens > 0).then_some(u.input_tokens as usize),
        output_tokens: (u.output_tokens > 0).then_some(u.output_tokens as usize),
        total_tokens: (u.total_tokens > 0).then_some(u.total_tokens as usize),
        cached_tokens: None,
    }
}

/// Convert a rig completion error into a ri-agent [`ProviderError`].
///
/// Preserves the HTTP status (recoverable from rig's transport errors) so the
/// UI can render provider-appropriate messages for auth, rate-limit, and server
/// failures instead of a generic "could not process the request".
fn to_provider_error(e: CompletionError) -> ProviderError {
    let source = "openai-compatible";
    let status = e.provider_response_status();
    let body = e.provider_response_body().unwrap_or("").trim().to_string();
    let message = if body.is_empty() { e.to_string() } else { body };
    match status {
        Some(s) if s == http::StatusCode::UNAUTHORIZED => {
            ProviderError::unauthorized(source, message)
        }
        Some(s) if s == http::StatusCode::FORBIDDEN => ProviderError::forbidden(source, message),
        Some(s) if s == http::StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::rate_limited(source, message)
        }
        Some(s) if s.is_server_error() => ProviderError::server_error(source, s.as_u16(), message),
        Some(s) => ProviderError::new(ProviderErrorKind::Other, Some(s.as_u16()), source, message),
        None if matches!(e, CompletionError::HttpError(_)) => {
            ProviderError::network(source, message)
        }
        None => ProviderError::other(source, message),
    }
}

// ── LlmProvider impl ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamPhase {
    Idle,
    InToolCall,
}

/// Deltas that are still accumulating partial JSON arguments for display.
fn yield_tool_call_delta(
    events: &mut Vec<LlmEvent>,
    id: String,
    content: ToolCallDeltaContent,
    phase: &mut StreamPhase,
) {
    match content {
        ToolCallDeltaContent::Name(name) => {
            *phase = StreamPhase::InToolCall;
            events.push(LlmEvent::ToolCallStart { id, name });
        }
        ToolCallDeltaContent::Delta(partial_json) => {
            if *phase == StreamPhase::Idle {
                // Start seen elsewhere; keep going with the accumulated id.
                *phase = StreamPhase::InToolCall;
            }
            events.push(LlmEvent::ToolCallArgsDelta { id, partial_json });
        }
    }
}

impl super::LlmProvider for RigOpenAiProvider {
    fn stream_chat(&self, messages: Vec<Message>, context: LlmRequestContext) -> LlmStream {
        self.stream_chat_with_tools(messages, vec![], context)
    }

    fn stream_chat_with_tools(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        _context: LlmRequestContext,
    ) -> LlmStream {
        let client = self.client.clone();
        let model_name = self.model.clone();
        let rig_messages = to_rig_messages(&messages);
        let rig_tools = to_rig_tools(&tools);

        Box::pin(stream! {
            let model = client.completion_model(model_name.clone());
            let request = CompletionRequest {
                model: None,
                preamble: None,
                chat_history: OneOrMany::from_iter_optional(rig_messages)
                    .unwrap_or_else(|| OneOrMany::one(RigMessage::user(""))),
                documents: vec![],
                tools: rig_tools,
                temperature: None,
                max_tokens: None,
                tool_choice: None,
                additional_params: None,
                output_schema: None,
                record_telemetry_content: false,
            };

            let mut stream = match model.stream(request).await {
                Ok(s) => s,
                Err(e) => {
                    yield LlmEvent::Error(to_provider_error(e));
                    return;
                }
            };

            let mut phase = StreamPhase::Idle;
            let mut latest_usage: Option<UsageStats> = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamedAssistantContent::Text(text)) => {
                        let content = text.text;
                        if content.is_empty() {
                            continue;
                        }
                        phase = StreamPhase::Idle;
                        yield LlmEvent::Token {
                            text: content,
                            phase: AssistantPhase::Unknown,
                        };
                    }
                    Ok(StreamedAssistantContent::Reasoning(r)) => {
                        let text = r.display_text();
                        if !text.is_empty() {
                            yield LlmEvent::ThinkingToken(text);
                        }
                    }
                    Ok(StreamedAssistantContent::ReasoningDelta { reasoning, .. }) => {
                        if !reasoning.is_empty() {
                            yield LlmEvent::ThinkingToken(reasoning);
                        }
                    }
                    Ok(StreamedAssistantContent::ToolCallDelta { id, content, .. }) => {
                        let mut buf = Vec::new();
                        yield_tool_call_delta(&mut buf, id, content, &mut phase);
                        for ev in buf {
                            yield ev;
                        }
                    }
                    Ok(StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                        yield LlmEvent::ToolCall {
                            id: tool_call.id.clone(),
                            name: tool_call.function.name.clone(),
                            args: tool_call.function.arguments.clone(),
                        };
                    }
                    Ok(StreamedAssistantContent::Final(final_resp)) => {
                        let usage = final_resp.token_usage();
                        latest_usage = Some(to_usage_stats(&usage));
                    }
                    Ok(StreamedAssistantContent::Unknown(_)) => {
                        // Unmodeled provider output — ignore for now.
                    }
                    Err(e) => {
                        yield LlmEvent::Error(to_provider_error(e));
                        return;
                    }
                }
            }

            if let Some(usage) = latest_usage {
                yield LlmEvent::Usage(usage);
            }
            yield LlmEvent::Done;
        })
    }

    fn list_models(&self) -> ModelListFuture {
        Box::pin(async { Ok(vec![]) })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(content: &str) -> Message {
        Message::user(content)
    }

    fn assistant_msg(content: &str) -> Message {
        Message::assistant(content)
    }

    #[test]
    fn maps_roles_and_groups_tool_calls() {
        let messages = vec![
            Message::system("be concise"),
            user_msg("list files"),
            assistant_msg("sure"),
            Message::tool_call(
                "call_1",
                "find_files",
                serde_json::json!({ "pattern": "*" }),
            ),
            Message::tool_result("call_1", "ok", false),
            user_msg("thanks"),
        ];
        let rig = to_rig_messages(&messages);

        // system, user, assistant(merged w/ tool call), user(tool result), user
        assert_eq!(rig.len(), 5);
        assert!(matches!(&rig[0], RigMessage::System { .. }));
        assert!(matches!(&rig[1], RigMessage::User { .. }));
        match &rig[2] {
            RigMessage::Assistant { content, .. } => {
                let parts: Vec<_> = content.clone().into_iter().collect();
                assert_eq!(parts.len(), 2, "text + one tool call");
                assert!(matches!(parts[0], rig_message::AssistantContent::Text(_)));
                assert!(matches!(
                    parts[1],
                    rig_message::AssistantContent::ToolCall(_)
                ));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        assert!(matches!(&rig[4], RigMessage::User { .. }));
    }

    #[test]
    fn maps_tools() {
        let tools = vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "reads a file".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
            streaming_field: None,
        }];
        let rig = to_rig_tools(&tools);
        assert_eq!(rig.len(), 1);
        assert_eq!(rig[0].name, "read_file");
        assert_eq!(rig[0].parameters["type"], "object");
    }

    #[test]
    fn usage_stats_maps_zeros_to_none() {
        let u = RigUsage::new();
        let s = to_usage_stats(&u);
        assert_eq!(s.input_tokens, None);
        assert_eq!(s.output_tokens, None);
        assert_eq!(s.total_tokens, None);
    }

    #[test]
    fn maps_rig_errors_to_typed_provider_errors() {
        use rig_core::completion::CompletionError;
        use rig_core::http_client::Error as HttpError;

        // Unauthorized from a non-2xx HTTP response (auth failures common with
        // expired API keys).
        let unauthorized =
            CompletionError::from_http_response(http::StatusCode::UNAUTHORIZED, "invalid api key");
        let err = to_provider_error(unauthorized);
        assert_eq!(err.kind, ProviderErrorKind::Unauthorized);
        assert_eq!(err.status_code, Some(401));
        assert!(err.message.contains("invalid api key"));

        // 429 rate limit.
        let limited =
            CompletionError::from_http_response(http::StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert_eq!(
            to_provider_error(limited).kind,
            ProviderErrorKind::RateLimited
        );

        // 5xx server error.
        let server = CompletionError::from_http_response(http::StatusCode::BAD_GATEWAY, "boom");
        let err = to_provider_error(server);
        assert_eq!(err.kind, ProviderErrorKind::ServerError);
        assert_eq!(err.status_code, Some(502));

        // Transport-level failure -> network.
        let network = CompletionError::HttpError(HttpError::Instance(Box::new(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "conn refused"),
        )));
        assert_eq!(to_provider_error(network).kind, ProviderErrorKind::Network);

        // Plain provider diagnostic -> other.
        let other = CompletionError::ProviderError("model not found".to_string());
        assert_eq!(to_provider_error(other).kind, ProviderErrorKind::Other);
    }
}
