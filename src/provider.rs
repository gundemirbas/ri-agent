use std::sync::Arc;

use crate::{
    config::RiConfig,
    llm::{
        LlmProvider,
        rig_provider::{RigOpenAiApi, RigOpenAiProvider},
        test_provider::TestProvider,
    },
    provider_instance::{ApiType, BackendPreset, ProviderInstance},
    thinking::ThinkingLevel,
};

/// Parse a configured `output_schema` (stored as raw JSON in config.toml)
/// into rig's schema type, so it can drive structured output. Invalid JSON
/// Schemas fail provider construction with a contextual message.
fn parse_output_schema(
    provider_id: &str,
    value: &Option<serde_json::Value>,
) -> anyhow::Result<Option<rig_core::schemars::Schema>> {
    match value {
        None => Ok(None),
        Some(value) => serde_json::from_value::<rig_core::schemars::Schema>(value.clone())
            .map(Some)
            .map_err(|e| {
                anyhow::anyhow!("provider '{provider_id}' has an invalid output_schema: {e}")
            }),
    }
}

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
    _config: &RiConfig,
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
                .with_reasoning_effort(thinking.to_reasoning_effort())
                .with_completion_options(
                    instance.temperature,
                    instance.max_tokens,
                    parse_output_schema(&instance.id, &instance.output_schema)?,
                );
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
    fn parse_output_schema_accepts_valid_json_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "title": "Answer",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"]
        });
        let parsed = parse_output_schema("p", &Some(schema)).unwrap();
        assert!(parsed.is_some());
    }

    #[test]
    fn parse_output_schema_rejects_non_object_value() {
        // A top-level scalar is not a valid JSON Schema — the Schema
        // deserializer rejects everything except objects and booleans.
        let schema = serde_json::json!(42);
        let err = parse_output_schema("p", &Some(schema)).unwrap_err();
        assert!(err.to_string().contains("invalid output_schema"));
    }

    #[test]
    fn parse_output_schema_none_round_trips() {
        assert!(parse_output_schema("p", &None).unwrap().is_none());
    }

    #[test]
    fn build_provider_for_instance_accepts_completion_options() {
        use crate::thinking::ThinkingLevel;

        let mut instance = compat(ApiType::OpenAiCompatible);
        instance.base_url = Some("http://localhost:9999".to_string());
        instance.api_key = Some("sk-test".to_string());
        instance.temperature = Some(0.2);
        instance.max_tokens = Some(100);
        instance.output_schema = Some(serde_json::json!({
            "type": "object",
            "title": "Summary"
        }));
        let p = build_provider_for_instance(
            &instance,
            ThinkingLevel::Off,
            &crate::config::RiConfig::default(),
        );
        assert!(
            p.is_ok(),
            "builder should accept completion options: {:?}",
            p.err()
        );
    }

    #[test]
    fn build_provider_for_instance_rejects_bad_output_schema() {
        use crate::thinking::ThinkingLevel;

        let mut instance = compat(ApiType::OpenAiCompatible);
        instance.base_url = Some("http://localhost:9999".to_string());
        instance.api_key = Some("sk-test".to_string());
        instance.output_schema = Some(serde_json::json!(42));
        let err = match build_provider_for_instance(
            &instance,
            ThinkingLevel::Off,
            &crate::config::RiConfig::default(),
        ) {
            Ok(_) => panic!("expected build to fail on invalid schema"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("invalid output_schema"));
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
