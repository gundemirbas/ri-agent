use std::sync::Arc;

use crate::{
    config::XiConfig,
    llm::{LlmProvider, rig_provider::RigOpenAiProvider, test_provider::TestProvider},
    provider_instance::{ApiType, BackendPreset, ProviderInstance},
    thinking::ThinkingLevel,
};

/// Whether a provider supports "thinking" (reasoning effort) control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSupport {
    Applied,
    Ignored(&'static str),
}

/// Return the thinking support level for a named provider instance.
pub fn thinking_support_for_instance(instance: &ProviderInstance, _model: &str) -> ThinkingSupport {
    match instance.api_type {
        ApiType::OpenAiCompatible => {
            // Generic OpenAI-compatible endpoints (e.g. DeepSeek) may or may not
            // support `reasoning_effort`.  Many don't — they still produce
            // `reasoning_content` in responses autonomously, but sending the
            // parameter triggers a 400 error.  Mark thinking as unsupported so
            // the parameter is not sent; the model's autonomous reasoning
            // tokens are still streamed live via rig's reasoning deltas.
            ThinkingSupport::Ignored(
                "generic openai-compatible: reasoning_effort not reliably supported",
            )
        }
        ApiType::Test => ThinkingSupport::Ignored("test provider does not support thinking"),
    }
}

/// Build a provider for a named [`ProviderInstance`].
pub fn build_provider_for_instance(
    instance: &ProviderInstance,
    _thinking: ThinkingLevel,
    _config: &XiConfig,
) -> anyhow::Result<Arc<dyn LlmProvider + Send + Sync>> {
    let model = instance.effective_model();

    match instance.backend_preset {
        BackendPreset::OpenAiCompatible => {
            let base_url = instance.base_url.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "No base URL for OpenAI-compatible provider '{}'. Configure it first.",
                    instance.id
                )
            })?;
            let api_key = instance.api_key.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing API key for provider '{}'. Set api_key in config.",
                    instance.id
                )
            })?;
            let p = RigOpenAiProvider::new(base_url, model, api_key).map_err(|e| {
                anyhow::anyhow!(
                    "failed to build OpenAI-compatible provider '{}': {e}",
                    instance.id
                )
            })?;
            Ok(Arc::new(p))
        }
        BackendPreset::Test => Ok(Arc::new(TestProvider::new())),
    }
}

#[cfg(test)]
mod tests {}
