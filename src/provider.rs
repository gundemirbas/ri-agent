use std::sync::Arc;

use crate::{
    config::XiConfig,
    llm::{
        LlmProvider,
        rig_provider::{RigOpenAiApi, RigOpenAiProvider},
        test_provider::TestProvider,
    },
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
        ApiType::OpenAiResponses | ApiType::OpenAiCompatible => {
            // Both OpenAI wire protocols accept a reasoning-effort control.
            ThinkingSupport::Applied
        }
        ApiType::Test => ThinkingSupport::Ignored("test provider does not support thinking"),
    }
}

/// Build a provider for a named [`ProviderInstance`].
pub fn build_provider_for_instance(
    instance: &ProviderInstance,
    thinking: ThinkingLevel,
    _config: &XiConfig,
) -> anyhow::Result<Arc<dyn LlmProvider + Send + Sync>> {
    let model = instance.effective_model();

    match instance.backend_preset {
        BackendPreset::OpenAiCompatible => {
            let base_url = instance.base_url.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "No base URL for OpenAI provider '{}'. Configure it first.",
                    instance.id
                )
            })?;
            let api_key = instance.api_key.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing API key for provider '{}'. Set api_key in config.",
                    instance.id
                )
            })?;
            let api_type = match instance.api_type {
                ApiType::OpenAiResponses => RigOpenAiApi::Responses,
                ApiType::OpenAiCompatible | ApiType::Test => RigOpenAiApi::Completions,
            };
            let p = RigOpenAiProvider::new(api_type, base_url, model, api_key)
                .map_err(|e| {
                    anyhow::anyhow!("failed to build OpenAI provider '{}': {e}", instance.id)
                })?
                .with_reasoning_effort(thinking.to_reasoning_effort());
            Ok(Arc::new(p))
        }
        BackendPreset::Test => Ok(Arc::new(TestProvider::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compat(api_type: ApiType) -> ProviderInstance {
        let mut i = ProviderInstance::new("test", BackendPreset::OpenAiCompatible);
        i.api_type = api_type;
        i
    }

    #[test]
    fn openai_protocols_support_thinking() {
        assert_eq!(
            thinking_support_for_instance(&compat(ApiType::OpenAiResponses), "gpt-5"),
            ThinkingSupport::Applied
        );
        assert_eq!(
            thinking_support_for_instance(&compat(ApiType::OpenAiCompatible), "gpt-4o"),
            ThinkingSupport::Applied
        );
    }
}
