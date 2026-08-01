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
use rig_core::client::{CompletionClient, ModelListingClient};
use rig_core::completion::request::Usage as RigUsage;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, GetTokenUsage,
    ToolDefinition as RigToolDefinition,
};
use rig_core::http_client::HeaderMap;
use rig_core::message as rig_message;
use rig_core::message::Message as RigMessage;
use rig_core::model::ModelListingError;
use rig_core::providers::openai::{Client as ResponsesClient, CompletionsClient};
use rig_core::streaming::{StreamedAssistantContent, ToolCallDeltaContent};

use super::{
    AssistantPhase, LlmEvent, LlmStream, Message, ModelListFuture, Role, ToolDefinition, UsageStats,
};
use crate::llm::{ProviderError, ProviderErrorKind};
use crate::provider_instance::ensure_v1_prefix;

/// Which OpenAI wire protocol this provider uses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RigOpenAiApi {
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// Chat Completions / OpenAI-compatible (`/v1/chat/completions`).
    Completions,
}

/// The underlying rig OpenAI transport (Responses or Chat Completions).
#[derive(Clone)]
enum RigClient {
    Responses(ResponsesClient),
    Completions(CompletionsClient),
}

/// OpenAI provider built on rig, supporting both the Responses API and the
/// Chat Completions ("OpenAI-compatible") protocol.
pub struct RigOpenAiProvider {
    client: RigClient,
    model: String,
    base_url: String,
    api_key: String,
    /// Alternate base URL (with/without the `/v1` prefix, whichever the
    /// primary does not use). Tried once on a pre-content 404 so that both
    /// `…/v1` and pathless endpoint styles work.
    alternate_base_url: Option<String>,
    /// Per-request HTTP headers passed at construction, kept so a fallback
    /// client can be rebuilt with the same headers.
    headers: Vec<(String, String)>,
    reasoning_effort: Option<&'static str>,
    /// Sampling temperature forwarded to every `CompletionRequest`.
    temperature: Option<f64>,
    /// Maximum output tokens forwarded to every `CompletionRequest`.
    max_tokens: Option<u64>,
    /// Optional JSON Schema (structured output) forwarded to
    /// `CompletionRequest::output_schema`.
    output_schema: Option<rig_core::schemars::Schema>,
}

impl RigOpenAiProvider {
    /// Create a provider for an OpenAI endpoint using `api_type`.
    ///
    /// `base_url` may omit the `/v1` version prefix — it is added
    /// automatically when the URL has no path of its own (e.g.
    /// `https://api.openai.com` → `https://api.openai.com/v1`; rig then appends
    /// `/chat/completions` or `/responses`).
    pub fn new(
        api_type: RigOpenAiApi,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Self::new_with_headers(api_type, base_url, model, api_key, vec![])
    }

    /// Create a provider with extra per-request HTTP headers.
    pub fn new_with_headers(
        api_type: RigOpenAiApi,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        let base_url = ensure_v1_prefix(&base_url.into());
        let alternate_base_url = alternate_base_url(&base_url);
        let api_key: String = api_key.into();
        let header_map = header_map_from_pairs(headers.clone());
        let client = match api_type {
            RigOpenAiApi::Responses => {
                RigClient::Responses(build_responses_client(&base_url, &api_key, header_map)?)
            }
            RigOpenAiApi::Completions => {
                RigClient::Completions(build_completions_client(&base_url, &api_key, header_map)?)
            }
        };
        Ok(Self {
            client,
            model: model.into(),
            base_url,
            alternate_base_url,
            api_key,
            headers,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
            output_schema: None,
        })
    }

    /// Set the `reasoning_effort` request parameter.  `None` omits it.
    ///
    /// For the Responses API this is translated to the `reasoning.effort`
    /// field; for Chat Completions it is sent as the top-level
    /// `reasoning_effort` parameter.
    pub fn with_reasoning_effort(mut self, effort: Option<&'static str>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Set per-request completion options forwarded to every
    /// [`CompletionRequest`]: sampling `temperature`, maximum output tokens,
    /// and an optional output schema (structured output).  `None` entries omit
    /// the corresponding field.
    pub fn with_completion_options(
        mut self,
        temperature: Option<f64>,
        max_tokens: Option<u64>,
        output_schema: Option<rig_core::schemars::Schema>,
    ) -> Self {
        self.temperature = temperature;
        self.max_tokens = max_tokens;
        self.output_schema = output_schema;
        self
    }
}

fn header_map_from_pairs(headers: Vec<(String, String)>) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let (Ok(k), Ok(v)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(&v),
        ) else {
            continue;
        };
        map.insert(k, v);
    }
    map
}

/// Whether a provider error is an HTTP 404 (endpoint not found) — the
/// signal used to retry the alternate base URL.
fn is_not_found(e: &CompletionError) -> bool {
    e.provider_response_status() == Some(http::StatusCode::NOT_FOUND)
}

/// The opposite of the configured base URL's `/v1` treatment: if the base
/// ends with `/v1`, the alternate drops it; otherwise it appends `/v1`.
fn alternate_base_url(base_url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    let path = parsed.path().trim_end_matches('/');
    let alt = if path.ends_with("/v1") {
        let mut url = parsed.clone();
        let base = path.strip_suffix("/v1").unwrap_or_default();
        url.set_path(if base.is_empty() { "/" } else { base });
        url
    } else {
        reqwest::Url::parse(&format!("{}/v1", base_url.trim_end_matches('/'))).ok()?
    };
    let alt_str = alt.to_string().trim_end_matches('/').to_string();
    (alt_str != base_url).then_some(alt_str)
}

fn build_responses_client(
    base_url: &str,
    api_key: &str,
    headers: HeaderMap,
) -> anyhow::Result<ResponsesClient> {
    let mut builder = ResponsesClient::builder().api_key(api_key);
    builder = builder.base_url(base_url);
    if !headers.is_empty() {
        builder = builder.http_headers(headers);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build OpenAI Responses client: {e}"))
}

fn build_completions_client(
    base_url: &str,
    api_key: &str,
    headers: HeaderMap,
) -> anyhow::Result<CompletionsClient> {
    let mut builder = CompletionsClient::builder().api_key(api_key);
    builder = builder.base_url(base_url);
    if !headers.is_empty() {
        builder = builder.http_headers(headers);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build OpenAI-compatible client: {e}"))
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
                let mut tr_content = Vec::new();
                let mut pushed_image = false;
                if let Some(img) = &msg.image_data
                    && let Some(media_type) = to_image_media_type(&img.mime_type)
                {
                    tr_content.push(rig_message::ToolResultContent::image_base64(
                        img.base64.clone(),
                        Some(media_type),
                        None,
                    ));
                    pushed_image = true;
                }
                // When a binary image was attached and encoded, send it instead
                // of the `[image]` text placeholder so vision-capable models get
                // the real pixels via rig's `ToolResultContent::Image`.
                if !pushed_image && !msg.content.is_empty() {
                    tr_content.push(rig_message::ToolResultContent::text(msg.content.clone()));
                }
                out.push(RigMessage::User {
                    content: OneOrMany::one(rig_message::UserContent::tool_result(
                        id,
                        OneOrMany::from_iter_optional(tr_content).unwrap_or_else(|| {
                            OneOrMany::one(rig_message::ToolResultContent::text(""))
                        }),
                    )),
                });
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

/// Map a ri-agent image MIME type onto rig's [`ImageMediaType`]
/// (`rig_message::ImageMediaType`), returning `None` for unsupported types.
fn to_image_media_type(mime: &str) -> Option<rig_message::ImageMediaType> {
    match mime {
        "image/jpeg" => Some(rig_message::ImageMediaType::JPEG),
        "image/png" => Some(rig_message::ImageMediaType::PNG),
        "image/gif" => Some(rig_message::ImageMediaType::GIF),
        "image/webp" => Some(rig_message::ImageMediaType::WEBP),
        "image/heic" => Some(rig_message::ImageMediaType::HEIC),
        "image/heif" => Some(rig_message::ImageMediaType::HEIF),
        "image/svg+xml" => Some(rig_message::ImageMediaType::SVG),
        _ => None,
    }
}

/// Map rig token usage onto ri-agent [`UsageStats`].
fn to_usage_stats(u: &RigUsage) -> UsageStats {
    UsageStats {
        input_tokens: (u.input_tokens > 0).then_some(u.input_tokens as usize),
        output_tokens: (u.output_tokens > 0).then_some(u.output_tokens as usize),
        total_tokens: (u.total_tokens > 0).then_some(u.total_tokens as usize),
        // OpenAI (both wire protocols) reports cache hits as a subset of the
        // prompt tokens via `prompt_tokens_details.cached_tokens` /
        // `input_tokens_details.cached_tokens`; rig folds that into
        // `cached_input_tokens`. Keeping the subset relationship intact means
        // `UsageStats::used_tokens` won't double-count (`cached <= input`).
        cached_tokens: (u.cached_input_tokens > 0).then_some(u.cached_input_tokens as usize),
        // OpenAI o-series reports reasoning ("thinking") output tokens
        // separately; rig folds them into `reasoning_tokens`.
        reasoning_tokens: (u.reasoning_tokens > 0).then_some(u.reasoning_tokens as usize),
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

/// Build a [`ProviderError`] from a raw HTTP status code returned by the model
/// list endpoint.
fn provider_error_for_status(status: u16, message: String) -> ProviderError {
    match status {
        401 => ProviderError::unauthorized("openai", message),
        403 => ProviderError::forbidden("openai", message),
        429 => ProviderError::rate_limited("openai", message),
        500..=599 => ProviderError::server_error("openai", status, message),
        other => ProviderError::new(ProviderErrorKind::Other, Some(other), "openai", message),
    }
}

/// Map a rig [`ModelListingError`] onto a ri-agent [`ProviderError`].
fn listing_to_provider_error(e: ModelListingError) -> ProviderError {
    match e {
        ModelListingError::ApiError {
            status_code,
            message,
        } => provider_error_for_status(status_code, message),
        ModelListingError::AuthError { message } => ProviderError::unauthorized("openai", message),
        ModelListingError::RateLimitError { message } => {
            ProviderError::rate_limited("openai", message)
        }
        ModelListingError::ServiceUnavailable { message } => {
            ProviderError::server_error("openai", 503, message)
        }
        // rig's http layer short-circuits non-2xx responses into a transport
        // error that lands here as `RequestError`; the status code is still
        // embedded in the message, so recover it to keep auth/rate-limit/serve
        // failures typed. Status-less entries are genuinely transport errors.
        ModelListingError::RequestError { message } => match listing_message_status(&message) {
            Some(status) => provider_error_for_status(status, message),
            None => ProviderError::network("openai", message),
        },
        ModelListingError::ParseError { message } | ModelListingError::UnknownError { message } => {
            ProviderError::other("openai", message)
        }
    }
}

/// Extract an HTTP status code from rig's `InvalidStatusCode*` transport
/// message format — the only typed signal rig preserves for non-2xx responses.
fn listing_message_status(message: &str) -> Option<u16> {
    let rest = message.strip_prefix("Invalid status code")?;
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
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

/// Map a single rig stream item onto ri-agent [`LlmEvent`]s, preserving live
/// text, reasoning, and tool-call argument deltas.
///
/// `R` is the provider raw-response type; the only provider-specific part is
/// its usage, extracted via [`GetTokenUsage`].
fn map_stream_item<R: GetTokenUsage>(
    item: StreamedAssistantContent<R>,
    phase: &mut StreamPhase,
) -> (Vec<LlmEvent>, Option<UsageStats>) {
    let mut events = Vec::new();
    let mut usage = None;
    match item {
        StreamedAssistantContent::Text(text) => {
            let content = text.text;
            if !content.is_empty() {
                *phase = StreamPhase::Idle;
                events.push(LlmEvent::Token {
                    text: content,
                    phase: AssistantPhase::Unknown,
                });
            }
        }
        StreamedAssistantContent::Reasoning(r) => {
            let text = r.display_text();
            if !text.is_empty() {
                events.push(LlmEvent::ThinkingToken(text));
            }
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            if !reasoning.is_empty() {
                events.push(LlmEvent::ThinkingToken(reasoning));
            }
        }
        StreamedAssistantContent::ToolCallDelta { id, content, .. } => {
            yield_tool_call_delta(&mut events, id, content, phase);
        }
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            events.push(LlmEvent::ToolCall {
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
                args: tool_call.function.arguments.clone(),
            });
        }
        StreamedAssistantContent::Final(final_resp) => {
            let u = final_resp.token_usage();
            usage = Some(to_usage_stats(&u));
        }
        StreamedAssistantContent::Unknown(_) => {
            // Unmodeled provider output — ignore for now.
        }
    }
    (events, usage)
}

// ── Streaming driver (shared by both wire protocols) ─────────────────────────

/// Build a `CompletionRequest` from the prepared rig messages/tools plus the
/// protocol-specific `additional_params` (e.g. the reasoning-effort encoding).
fn full_completion_request(
    messages: &[RigMessage],
    tools: &[RigToolDefinition],
    additional_params: Option<serde_json::Value>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    output_schema: Option<rig_core::schemars::Schema>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::from_iter_optional(messages.to_vec())
            .unwrap_or_else(|| OneOrMany::one(RigMessage::user(""))),
        documents: vec![],
        tools: tools.to_vec(),
        temperature,
        max_tokens,
        tool_choice: None,
        additional_params,
        output_schema,
        record_telemetry_content: false,
    }
}

/// Single shared streaming driver used by both the Responses and Chat
/// Completions backends.
///
/// `C` is the concrete rig client (`ResponsesClient` or `CompletionsClient`).
/// `make_request` builds the protocol-specific [`CompletionRequest`] (including
/// its reasoning-effort `additional_params` shape) and `rebuild` recreates a
/// fully configured client of the same type against the alternate base URL, so
/// a pre-content 404 transparently falls back to a pathless (`/v1`-less)
/// endpoint without tearing the stream down.
fn stream_with_retry<C, MakeReq, Rebuild>(
    mut client: C,
    model_name: String,
    make_request: MakeReq,
    rebuild: Rebuild,
) -> LlmStream
where
    C: CompletionClient + Clone + Send + 'static,
    C::CompletionModel: CompletionModel<Client = C> + Send,
    <<C as CompletionClient>::CompletionModel as CompletionModel>::StreamingResponse:
        Clone + Send + Unpin + GetTokenUsage + 'static,
    MakeReq: Fn() -> CompletionRequest + Send + 'static,
    Rebuild: Fn() -> anyhow::Result<C> + Send + 'static,
{
    Box::pin(stream! {
        let mut retried = false;
        let mut emitted = false;
        let mut latest_usage: Option<UsageStats> = None;

        'stream: loop {
            let model = client.completion_model(model_name.clone());
            let mut stream = match model.stream(make_request()).await {
                Ok(s) => s,
                Err(e) if !retried && is_not_found(&e) => {
                    if let Ok(c) = rebuild() {
                        client = c;
                        retried = true;
                        continue 'stream;
                    }
                    yield LlmEvent::Error(to_provider_error(e));
                    return;
                }
                Err(e) => {
                    yield LlmEvent::Error(to_provider_error(e));
                    return;
                }
            };

            let mut phase = StreamPhase::Idle;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(item) => {
                        emitted = true;
                        let (events, usage) = map_stream_item(item, &mut phase);
                        latest_usage = usage.or(latest_usage);
                        for ev in events {
                            yield ev;
                        }
                    }
                    Err(e) if !retried && !emitted && is_not_found(&e) => {
                        if let Ok(c) = rebuild() {
                            client = c;
                            retried = true;
                            continue 'stream;
                        }
                        yield LlmEvent::Error(to_provider_error(e));
                        return;
                    }
                    Err(e) => {
                        yield LlmEvent::Error(to_provider_error(e));
                        return;
                    }
                }
            }
            break 'stream;
        }

        if let Some(usage) = latest_usage {
            yield LlmEvent::Usage(usage);
        }
        yield LlmEvent::Done;
    })
}

// ── Model listing via rig ─────────────────────────────────────────────────────

/// Which OpenAI wire protocol a rig client speaks.
fn client_protocol(client: &RigClient) -> RigOpenAiApi {
    match client {
        RigClient::Responses(_) => RigOpenAiApi::Responses,
        RigClient::Completions(_) => RigOpenAiApi::Completions,
    }
}

/// Rebuild a rig client of the same protocol against a different base URL
/// (used to retry the alternate `/v1`/pathless endpoint).
fn rebuild_client(
    api_type: RigOpenAiApi,
    base_url: &str,
    api_key: &str,
    headers: &[(String, String)],
) -> anyhow::Result<RigClient> {
    let map = header_map_from_pairs(headers.to_vec());
    match api_type {
        RigOpenAiApi::Responses => Ok(RigClient::Responses(build_responses_client(
            base_url, api_key, map,
        )?)),
        RigOpenAiApi::Completions => Ok(RigClient::Completions(build_completions_client(
            base_url, api_key, map,
        )?)),
    }
}

/// Fetch the model list through rig's `ModelListingClient` (`GET /models`),
/// reduced to the model ids.
///
/// rig wires `ModelListingClient` only to the Responses client type; the
/// Completions client converts to it via [`CompletionsClient::responses_api`]
/// (same base URL and auth) because `/models` is protocol-independent.
async fn list_models_once(client: &RigClient) -> Result<Vec<String>, ProviderError> {
    let result = match client {
        RigClient::Responses(c) => c.list_models().await,
        RigClient::Completions(c) => c.clone().responses_api().list_models().await,
    };
    result
        .map(|list| list.into_iter().map(|m| m.id).collect())
        .map_err(listing_to_provider_error)
}

impl super::LlmProvider for RigOpenAiProvider {
    fn stream(&self, messages: Vec<Message>, tools: Vec<ToolDefinition>) -> LlmStream {
        let client = self.client.clone();
        let model_name = self.model.clone();
        let reasoning_effort = self.reasoning_effort;
        let temperature = self.temperature;
        let max_tokens = self.max_tokens;
        let output_schema = self.output_schema.clone();
        let api_key = self.api_key.clone();
        let alternate_base_url = self.alternate_base_url.clone();
        let headers = self.headers.clone();
        let rig_messages = to_rig_messages(&messages);
        let rig_tools = to_rig_tools(&tools);

        match client {
            RigClient::Responses(rc) => {
                // Responses API carries reasoning effort as `reasoning.effort`.
                let make_request = {
                    let messages = rig_messages.clone();
                    let tools = rig_tools.clone();
                    move || {
                        let additional = reasoning_effort
                            .map(|e| serde_json::json!({ "reasoning": { "effort": e } }));
                        full_completion_request(
                            &messages,
                            &tools,
                            additional,
                            temperature,
                            max_tokens,
                            output_schema.clone(),
                        )
                    }
                };
                let rebuild = {
                    let alt = alternate_base_url.clone();
                    let key = api_key.clone();
                    let headers = headers.clone();
                    move || {
                        build_responses_client(
                            alt.as_deref()
                                .ok_or_else(|| anyhow::anyhow!("no alternate base URL"))?,
                            &key,
                            header_map_from_pairs(headers.clone()),
                        )
                    }
                };
                stream_with_retry(rc, model_name.clone(), make_request, rebuild)
            }
            RigClient::Completions(cc) => {
                // Chat Completions sends reasoning effort as a top-level param.
                let make_request = {
                    let messages = rig_messages.clone();
                    let tools = rig_tools.clone();
                    move || {
                        let additional =
                            reasoning_effort.map(|e| serde_json::json!({ "reasoning_effort": e }));
                        full_completion_request(
                            &messages,
                            &tools,
                            additional,
                            temperature,
                            max_tokens,
                            output_schema.clone(),
                        )
                    }
                };
                let rebuild = {
                    let alt = alternate_base_url.clone();
                    let key = api_key.clone();
                    let headers = headers.clone();
                    move || {
                        build_completions_client(
                            alt.as_deref()
                                .ok_or_else(|| anyhow::anyhow!("no alternate base URL"))?,
                            &key,
                            header_map_from_pairs(headers.clone()),
                        )
                    }
                };
                stream_with_retry(cc, model_name, make_request, rebuild)
            }
        }
    }

    fn list_models(&self) -> ModelListFuture {
        let api_type = client_protocol(&self.client);
        let base_url = self.base_url.clone();
        let alternate_base_url = self.alternate_base_url.clone();
        let api_key = self.api_key.clone();
        let headers = self.headers.clone();
        Box::pin(async move {
            // Primary (normalized `/v1`) base URL first.
            let primary = match rebuild_client(api_type, &base_url, &api_key, &headers) {
                Ok(c) => list_models_once(&c).await,
                Err(e) => Err(ProviderError::other("openai", e.to_string())),
            };
            if let Ok(models) = primary {
                return Ok(models);
            }

            // On a provider error the `/v1` variant may be the wrong one — try
            // the alternate endpoint before giving up.
            if let Some(alt) = &alternate_base_url
                && let Ok(alt_client) = rebuild_client(api_type, alt, &api_key, &headers)
            {
                return list_models_once(&alt_client).await;
            }

            Err(primary.expect_err("primary succeeded above"))
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmProvider;

    fn user_msg(content: &str) -> Message {
        Message::user(content)
    }

    fn assistant_msg(content: &str) -> Message {
        Message::assistant(content)
    }

    #[test]
    fn constructor_appends_v1_to_pathless_base_url() {
        let p = RigOpenAiProvider::new(
            RigOpenAiApi::Responses,
            "http://localhost:9999",
            "main",
            "key",
        )
        .unwrap();
        assert_eq!(p.base_url, "http://localhost:9999/v1");
    }

    #[test]
    fn constructor_keeps_existing_path_untouched() {
        let p = RigOpenAiProvider::new(
            RigOpenAiApi::Responses,
            "http://localhost:9999/api",
            "main",
            "key",
        )
        .unwrap();
        assert_eq!(p.base_url, "http://localhost:9999/api");
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
    fn tool_result_with_image_maps_to_image_content() {
        let msg = Message::tool_result("call_1", "[image]", false).with_image_data(
            crate::llm::ImageData {
                base64: "aW1n".to_string(),
                mime_type: "image/png".to_string(),
            },
        );
        let rig = to_rig_messages(&[msg]);
        assert_eq!(rig.len(), 1);
        match &rig[0] {
            RigMessage::User { content } => {
                let parts: Vec<_> = content.clone().into_iter().collect();
                assert_eq!(parts.len(), 1, "image replaces the [image] placeholder");
                match &parts[0] {
                    rig_message::UserContent::ToolResult(tr) => {
                        let tr_parts: Vec<_> = tr.content.clone().into_iter().collect();
                        assert_eq!(tr_parts.len(), 1);
                        assert!(matches!(
                            tr_parts[0],
                            rig_message::ToolResultContent::Image(_)
                        ));
                    }
                    other => panic!("expected tool result, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_with_unsupported_image_type_keeps_placeholder_text() {
        let msg = Message::tool_result("call_1", "[image]", false).with_image_data(
            crate::llm::ImageData {
                base64: "AAAA".to_string(),
                mime_type: "image/tiff".to_string(),
            },
        );
        let rig = to_rig_messages(&[msg]);
        match &rig[0] {
            RigMessage::User { content } => {
                let parts: Vec<_> = content.clone().into_iter().collect();
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    rig_message::UserContent::ToolResult(tr) => {
                        let tr_parts: Vec<_> = tr.content.clone().into_iter().collect();
                        assert_eq!(tr_parts.len(), 1);
                        assert!(matches!(
                            tr_parts[0],
                            rig_message::ToolResultContent::Text(_)
                        ));
                    }
                    other => panic!("expected tool result, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn usage_stats_maps_zeros_to_none() {
        let u = RigUsage::new();
        let s = to_usage_stats(&u);
        assert_eq!(s.input_tokens, None);
        assert_eq!(s.output_tokens, None);
        assert_eq!(s.total_tokens, None);
        assert_eq!(s.cached_tokens, None);
        assert_eq!(s.reasoning_tokens, None);
    }

    #[test]
    fn usage_stats_maps_reasoning_tokens() {
        // OpenAI o-series reports reasoning output tokens separately; they must
        // flow through rig's generic Usage.reasoning_tokens.
        let mut u = RigUsage::new();
        u.input_tokens = 10;
        u.output_tokens = 20;
        u.total_tokens = 30;
        u.reasoning_tokens = 15;
        let s = to_usage_stats(&u);
        assert_eq!(s.reasoning_tokens, Some(15));
        assert_eq!(s.output_tokens, Some(20));
    }

    #[test]
    fn usage_stats_omits_zero_reasoning_tokens() {
        let mut u = RigUsage::new();
        u.output_tokens = 5;
        let s = to_usage_stats(&u);
        assert_eq!(s.reasoning_tokens, None);
    }

    #[test]
    fn usage_stats_maps_openai_cached_tokens() {
        // OpenAI reports cache hits as a subset of input tokens; the info bar
        // renders them as `[N⚡]` without double-counting the context usage.
        let mut u = RigUsage::new();
        u.input_tokens = 20_000;
        u.output_tokens = 200;
        u.total_tokens = 20_200;
        u.cached_input_tokens = 19_000;
        let s = to_usage_stats(&u);
        assert_eq!(s.cached_tokens, Some(19_000));
        // cached <= input → used_tokens stays at the total (no double count).
        assert_eq!(s.used_tokens(), Some(20_200));
    }

    #[test]
    fn usage_stats_omits_zero_cached_tokens() {
        let mut u = RigUsage::new();
        u.input_tokens = 10;
        let s = to_usage_stats(&u);
        assert_eq!(s.cached_tokens, None);
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

    #[tokio::test]
    async fn list_models_fetches_ids_from_models_endpoint() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "gpt-5", "object": "model", "created": 1730000000, "owned_by": "openai" },
                    { "id": "gpt-4o", "object": "model", "created": 1730000001, "owned_by": "openai" }
                ]
            })))
            .mount(&server)
            .await;

        let provider =
            RigOpenAiProvider::new(RigOpenAiApi::Completions, server.uri(), "gpt-4o", "sk-test")
                .unwrap();

        let models = provider.list_models().await.unwrap();
        assert_eq!(models, vec!["gpt-5".to_string(), "gpt-4o".to_string()]);
    }

    #[tokio::test]
    async fn list_models_maps_unauthorized_status() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
            .mount(&server)
            .await;

        let provider =
            RigOpenAiProvider::new(RigOpenAiApi::Responses, server.uri(), "gpt-5", "bad-token")
                .unwrap();

        let err = provider.list_models().await.unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Unauthorized);
        assert_eq!(err.status_code, Some(401));
    }

    #[tokio::test]
    async fn responses_stream_yields_text_tokens_with_sequence_numbers() {
        use futures_util::StreamExt;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        // OpenAI's real /responses stream includes sequence_number on every
        // event; rig requires it on each chunk.
        let sse = r#"data: {"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress"},"sequence_number":0}

data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"msg_1","role":"assistant","type":"message","status":"in_progress","content":[]}}

data: {"type":"response.content_part.added","sequence_number":2,"output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}

data: {"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"delta":"merha"}

data: {"type":"response.output_text.delta","sequence_number":4,"output_index":0,"content_index":0,"delta":"ba"}

data: {"type":"response.completed","sequence_number":5,"response":{"id":"resp_1","object":"response","created_at":1730000000,"status":"completed","model":"main","usage":{"input_tokens":4,"input_tokens_details":{"cached_tokens":2},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":6},"output":[{"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"merhaba","annotations":[]}]}]}}

data: [DONE]

"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = RigOpenAiProvider::new(
            RigOpenAiApi::Responses,
            format!("{}/v1", server.uri()),
            "main",
            "test",
        )
        .unwrap();
        let mut stream = provider.stream(vec![Message::user("naber")], vec![]);
        let mut tokens = Vec::new();
        let mut done = false;
        let mut usage = None;
        while let Some(ev) = stream.next().await {
            match ev {
                LlmEvent::Token { text, .. } => tokens.push(text),
                LlmEvent::Usage(u) => usage = Some(u),
                LlmEvent::Done => {
                    done = true;
                    break;
                }
                LlmEvent::Error(e) => eprintln!("EVENT ERROR: {:?} | {}", e.kind, e.message),
                _ => {}
            }
        }
        assert!(done, "expected Done");
        assert_eq!(tokens.join(""), "merhaba");
        // OpenAI's completed-event usage carries cached tokens; they must flow
        // through rig's generic Usage into UsageStats (and show as `[N⚡]`).
        assert_eq!(usage.as_ref().and_then(|u| u.cached_tokens), Some(2));
        assert_eq!(usage.map(|u| u.used_tokens()), Some(Some(6))); // no double count
        // Reasoning output tokens ride the same usage event → `[R…]` suffix.
        assert_eq!(usage.as_ref().and_then(|u| u.reasoning_tokens), Some(2));
    }

    #[tokio::test]
    async fn chat_completions_stream_yields_text_tokens() {
        use futures_util::StreamExt;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        // llama.cpp / OpenAI-compatible chat-completions SSE (the format most
        // third-party servers speak).
        let sse = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{"content":"merha"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{"content":"ba"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = RigOpenAiProvider::new(
            RigOpenAiApi::Completions,
            format!("{}/v1", server.uri()),
            "main",
            "test",
        )
        .unwrap();
        let mut stream = provider.stream(vec![Message::user("naber")], vec![]);
        let mut tokens = Vec::new();
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev {
                LlmEvent::Token { text, .. } => tokens.push(text),
                LlmEvent::Done => {
                    done = true;
                    break;
                }
                LlmEvent::Error(e) => eprintln!("EVENT ERROR: {:?} | {}", e.kind, e.message),
                _ => {}
            }
        }
        assert!(done, "expected Done");
        assert_eq!(tokens.join(""), "merhaba");
    }

    #[tokio::test]
    async fn chat_completions_request_carries_completion_options() {
        use futures_util::StreamExt;
        use rig_core::schemars::Schema;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let sse = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{"content":"merhaba"},"finish_reason":null}]}

data: [DONE]

"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let schema: Schema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "title": "Answer",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"]
        }))
        .unwrap();
        let provider = RigOpenAiProvider::new(
            RigOpenAiApi::Completions,
            format!("{}/v1", server.uri()),
            "main",
            "test",
        )
        .unwrap()
        .with_completion_options(Some(0.3), Some(50), Some(schema));

        // Drain the stream so the request is sent.
        let mut stream = provider.stream(vec![Message::user("naber")], vec![]);
        while let Some(ev) = stream.next().await {
            match ev {
                LlmEvent::Done => break,
                LlmEvent::Error(e) => eprintln!("EVENT ERROR: {:?} | {}", e.kind, e.message),
                _ => {}
            }
        }

        // temperature / max_tokens ride along directly; output_schema is mapped
        // to a strict json_schema response_format (no tools in this call).
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["max_tokens"], 50);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "Answer");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    /// A pathless base URL is normalized to `/v1`, so the primary request hits
    /// `/v1/...` and works out of the box.
    #[tokio::test]
    async fn pathless_base_url_auto_appends_v1() {
        use futures_util::StreamExt;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let sse = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{"content":"merhaba"},"finish_reason":null}]}

data: [DONE]

"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        // No `/v1` in the configured URL — it must be appended automatically.
        let provider =
            RigOpenAiProvider::new(RigOpenAiApi::Completions, server.uri(), "main", "test")
                .unwrap();
        let mut stream = provider.stream(vec![Message::user("naber")], vec![]);
        let mut tokens = Vec::new();
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev {
                LlmEvent::Token { text, .. } => tokens.push(text),
                LlmEvent::Done => {
                    done = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(done, "expected Done");
        assert_eq!(tokens.join(""), "merhaba");
    }

    /// When the primary (`/v1`) endpoint 404s but the pathless one works, the
    /// stream must transparently fall back and still produce the answer.
    #[tokio::test]
    async fn falls_back_to_alternate_base_on_404() {
        use futures_util::StreamExt;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let sse = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"main","choices":[{"index":0,"delta":{"content":"fallback"},"finish_reason":null}]}

data: [DONE]

"#;
        let server = MockServer::start().await;
        // Primary: `/v1/chat/completions` → 404 (server exposes API at root).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // Alternate: `/chat/completions` → succeeds.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        // Configured without `/v1`: normalization adds it as the primary; the
        // pathless URL becomes the fallback.
        let provider =
            RigOpenAiProvider::new(RigOpenAiApi::Completions, server.uri(), "main", "test")
                .unwrap();
        let mut stream = provider.stream(vec![Message::user("naber")], vec![]);
        let mut tokens = Vec::new();
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev {
                LlmEvent::Token { text, .. } => tokens.push(text),
                LlmEvent::Done => {
                    done = true;
                    break;
                }
                LlmEvent::Error(e) => eprintln!("EVENT ERROR: {:?} | {}", e.kind, e.message),
                _ => {}
            }
        }
        assert!(done, "expected Done after fallback");
        assert_eq!(tokens.join(""), "fallback");
    }
}
