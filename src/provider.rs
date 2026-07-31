use std::sync::Arc;

use crate::{
    auth::AuthStore,
    config::XiConfig,
    llm::{
        LlmProvider,
        gemini::{DEFAULT_BASE_URL as GEMINI_DEFAULT_BASE_URL, GeminiProvider},
        ollama::{self, OllamaProvider},
        rig_provider::RigOpenAiProvider,
        test_provider::TestProvider,
    },
    provider_instance::{ApiType, BackendPreset, ProviderInstance},
    thinking::ThinkingLevel,
};

const OPENROUTER_DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_REFERER: &str = "https://github.com/gundemirbas/ri-agent";
const OPENROUTER_TITLE: &str = "ri";

/// Whether a provider supports "thinking" (reasoning effort) control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSupport {
    Applied,
    Ignored(&'static str),
}

/// Return the thinking support level for a named provider instance.
pub fn thinking_support_for_instance(instance: &ProviderInstance, _model: &str) -> ThinkingSupport {
    match instance.api_type {
        ApiType::OpenAiResponses => ThinkingSupport::Applied,
        ApiType::GeminiNative => ThinkingSupport::Applied,
        ApiType::OpenAiCompatible => {
            if instance.backend_preset == BackendPreset::OpenAi
                || instance.backend_preset == BackendPreset::OpenRouter
            {
                // OpenAI API and OpenRouter support `reasoning_effort` in the
                // chat completions request body (OpenAI o-series convention).
                ThinkingSupport::Applied
            } else {
                // Generic OpenAI-compatible endpoints (e.g. DeepSeek) may or
                // may not support `reasoning_effort`.  Many don't — they still
                // produce `reasoning_content` in responses autonomously, but
                // sending the parameter triggers a 400 error.  Mark thinking
                // as unsupported so the parameter is not sent; the model's
                // autonomous reasoning tokens are still parsed from responses.
                ThinkingSupport::Ignored(
                    "generic openai-compatible: reasoning_effort not reliably supported",
                )
            }
        }
        ApiType::OllamaChatApi => {
            ThinkingSupport::Ignored("ollama provider does not support mapped thinking levels")
        }
        ApiType::Test => ThinkingSupport::Ignored("test provider does not support thinking"),
    }
}

/// Build a provider for a named [`ProviderInstance`], dispatching on its
/// [`ApiType`].
pub fn build_provider_for_instance(
    instance: &ProviderInstance,
    thinking: ThinkingLevel,
    _config: &XiConfig,
) -> anyhow::Result<Arc<dyn LlmProvider + Send + Sync>> {
    let model = instance.effective_model();

    match instance.backend_preset {
        // ── Cloud services with AuthStore credentials ─────────────────────
        BackendPreset::Gemini => {
            let store = AuthStore::load_default()?;
            let creds = store.get_gemini().ok_or_else(|| {
                anyhow::anyhow!("Not authenticated for gemini. Run /login gemini.")
            })?;
            let base_url = instance
                .base_url
                .clone()
                .unwrap_or_else(|| GEMINI_DEFAULT_BASE_URL.to_string());
            let p = GeminiProvider::new(base_url, model, creds.access_token, creds.project_id)
                .with_thinking_level(thinking.to_gemini_thinking_level());
            Ok(Arc::new(p))
        }

        // ── OpenAI-compatible cloud services (api_key in instance) ─────────
        BackendPreset::OpenAi | BackendPreset::OpenAiCompatible => {
            let base_url = instance
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let api_key = instance.api_key.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing API key for provider '{}'. Set api_key in config.",
                    instance.id
                )
            })?;
            let mut p = RigOpenAiProvider::new(base_url, model, api_key).map_err(|e| {
                anyhow::anyhow!(
                    "failed to build OpenAI-compatible provider '{}': {e}",
                    instance.id
                )
            })?;
            log::debug!(
                "provider build: id={} backend={:?} api={:?} thinking={:?}",
                instance.id,
                instance.backend_preset,
                instance.api_type,
                thinking,
            );
            // Only send reasoning_effort for OpenAI API; generic
            // openai-compatible endpoints (e.g. DeepSeek) may reject it.
            if instance.backend_preset == BackendPreset::OpenAi {
                p = p.with_reasoning_effort(thinking.to_reasoning_effort_string());
            }
            Ok(Arc::new(p))
        }
        BackendPreset::OpenRouter => {
            let base_url = instance
                .base_url
                .clone()
                .unwrap_or_else(|| OPENROUTER_DEFAULT_BASE_URL.to_string());
            let api_key = instance.api_key.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing API key for provider '{}'. Set api_key in config.",
                    instance.id
                )
            })?;
            let mut p = RigOpenAiProvider::new_with_headers(
                base_url,
                model,
                api_key,
                vec![
                    ("HTTP-Referer".to_string(), OPENROUTER_REFERER.to_string()),
                    ("X-Title".to_string(), OPENROUTER_TITLE.to_string()),
                ],
            )
            .map_err(|e| {
                anyhow::anyhow!("failed to build OpenRouter provider '{}': {e}", instance.id)
            })?;
            p = p.with_reasoning_effort(thinking.to_reasoning_effort_string());
            Ok(Arc::new(p))
        }

        // ── Ollama ────────────────────────────────────────────────────────
        BackendPreset::Ollama => {
            let base = instance
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Arc::new(OllamaProvider::new(base, model.to_string())))
        }
        BackendPreset::OllamaCom => {
            let base = instance
                .base_url
                .clone()
                .unwrap_or_else(|| "https://ollama.com".to_string());
            let api_key = instance.api_key.clone();
            let mut p = OllamaProvider::new(base, model.to_string());
            p.api_key = api_key;
            Ok(Arc::new(p))
        }

        // ── Open WebUI ────────────────────────────────────────────────────
        BackendPreset::OpenWebUi => {
            let base = instance.base_url.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "No base URL for Open WebUI provider '{}'. Configure it first.",
                    instance.id
                )
            })?;
            match instance.api_type {
                ApiType::OllamaChatApi => {
                    let api_base = format!("{}/ollama", base.trim_end_matches('/'));
                    Ok(Arc::new(OllamaProvider::new(api_base, model.to_string())))
                }
                _ => {
                    let api_base = format!("{}/api", base.trim_end_matches('/'));
                    let api_key = instance.api_key.clone().unwrap_or_default();

                    // Also try to populate the Ollama context-window cache.
                    // Open WebUI proxies the Ollama-native API at /ollama/api
                    // even when the OpenAI-compatible /api endpoint is used.
                    let model_owned = model.to_string();
                    let ollama_base = format!("{}/ollama", base.trim_end_matches('/'));
                    let api_key_for_task = if api_key.is_empty() {
                        None
                    } else {
                        Some(api_key.clone())
                    };
                    if OllamaProvider::cached_context_window(&model_owned).is_none() {
                        tokio::spawn(async move {
                            ollama::fetch_and_cache_running_contexts(
                                &ollama_base,
                                api_key_for_task.as_deref(),
                            )
                            .await;
                        });
                    }

                    Ok(Arc::new(
                        RigOpenAiProvider::new(api_base, model, api_key).map_err(|e| {
                            anyhow::anyhow!(
                                "failed to build Open WebUI provider '{}': {e}",
                                instance.id
                            )
                        })?,
                    ))
                }
            }
        }

        // ── Test ──────────────────────────────────────────────────────────
        BackendPreset::Test => Ok(Arc::new(TestProvider::new())),
    }
}

#[cfg(test)]
mod tests {
    use crate::thinking::{GeminiThinkingLevel, ThinkingLevel};

    #[test]
    fn shared_reasoning_effort_mapping_matches_responses_routes() {
        assert_eq!(ThinkingLevel::Off.to_reasoning_effort_string(), None);
        assert_eq!(
            ThinkingLevel::Minimal.to_reasoning_effort_string(),
            Some("minimal".to_string())
        );
        assert_eq!(
            ThinkingLevel::XHigh.to_reasoning_effort_string(),
            Some("xhigh".to_string())
        );
    }

    #[test]
    fn shared_gemini_mapping_preserves_provider_specific_clamp() {
        assert_eq!(ThinkingLevel::Off.to_gemini_thinking_level(), None);
        assert_eq!(
            ThinkingLevel::Medium.to_gemini_thinking_level(),
            Some(GeminiThinkingLevel::Medium)
        );
        assert_eq!(
            ThinkingLevel::XHigh.to_gemini_thinking_level(),
            Some(GeminiThinkingLevel::High)
        );
    }
}
