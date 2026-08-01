// Items in this module form the public provider-instance API.

/// API protocol/transport types that ri-agent knows how to speak.
///
/// OpenAI's two wire protocols are supported via `rig`: the newer **Responses**
/// API (`/v1/responses`) and the older Chat Completions / "OpenAI-compatible"
/// protocol (`/v1/chat/completions`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiType {
    /// OpenAI Responses API (`/v1/responses`).
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// Chat Completions protocol (`/v1/chat/completions`) — works with any
    /// OpenAI-compatible endpoint (DeepSeek, vLLM, local inference servers, …).
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    /// Internal only — used by the test provider. Never shown to users.
    Test,
}

impl ApiType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenAiResponses => "OpenAI Responses",
            Self::OpenAiCompatible => "OpenAI Completions",
            Self::Test => "Test",
        }
    }
}

/// Recognisable software / cloud services ri-agent supports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendPreset {
    /// Generic OpenAI-compatible endpoint
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    /// Internal test provider — never shown to users.
    Test,
}

/// URL normalization parameters for a backend preset that accepts a user-supplied URL.
pub struct UrlNormalization {
    /// Default scheme to prepend when none is present (e.g. `"https"`).
    pub default_scheme: &'static str,
    /// Input label shown next to the textarea (e.g. `"URL: "`).
    pub endpoint_label: &'static str,
}

impl UrlNormalization {
    /// Normalize a raw user-entered URL string using this preset's parameters.
    ///
    /// Returns `None` for empty/blank input or URLs that are still invalid after
    /// normalization.
    pub fn normalize(&self, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            format!("{}://{}", self.default_scheme, trimmed)
        };
        let url = reqwest::Url::parse(&with_scheme).ok()?;
        match url.scheme() {
            "http" | "https" => {}
            _ => return None,
        }
        url.host_str()?;
        Some(ensure_v1_prefix(url.as_ref()))
    }
}

/// Ensure an API base URL includes the version prefix real servers expect.
///
/// OpenAI-compatible servers expose their API under a `/v1` path (rig appends
/// `/chat/completions` or `/responses`). When the configured URL has no path of
/// its own (e.g. `https://api.openai.com` or `http://localhost:11434`), append
/// `/v1` so these bare endpoints work out of the box. URLs that already carry a
/// path (e.g. `/api` for Open WebUI) are left untouched.
pub fn ensure_v1_prefix(base_url: &str) -> String {
    match reqwest::Url::parse(base_url) {
        Ok(mut url) => {
            if url.path().is_empty() || url.path() == "/" {
                url.set_path("/v1");
            }
            url.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => base_url.trim_end_matches('/').to_string(),
    }
}

/// Metadata ri-agent keeps about a backend preset.
pub struct BackendPresetDef {
    /// Machine-readable id (matches `BackendPreset` serialisation).
    pub id: &'static str,
    /// Human-readable display label.
    pub label: &'static str,
    /// Whether the user is asked to choose an API type when adding this preset
    /// (i.e. the preset supports more than one selectable protocol).
    pub user_selects_api: bool,
    /// API types the user may pick from for this preset, in picker order.
    pub allowed_apis: &'static [ApiType],
    /// The recommended / default API type.
    pub default_api: ApiType,
    /// URL normalization parameters for presets that accept a user-supplied URL.
    /// `None` for presets with internal endpoints.
    pub url_normalization: Option<UrlNormalization>,
}

/// Static catalog of all supported backend presets.
pub const BACKEND_PRESET_CATALOG: &[BackendPresetDef] = &[
    BackendPresetDef {
        id: "openai-compatible",
        label: "OpenAI-compatible endpoint",
        user_selects_api: true,
        allowed_apis: &[ApiType::OpenAiResponses, ApiType::OpenAiCompatible],
        default_api: ApiType::OpenAiCompatible,
        url_normalization: Some(UrlNormalization {
            default_scheme: "https",
            endpoint_label: "URL: ",
        }),
    },
    BackendPresetDef {
        id: "test",
        label: "Test (UI exercise)",
        user_selects_api: false,
        allowed_apis: &[ApiType::Test],
        default_api: ApiType::Test,
        url_normalization: None,
    },
];

impl BackendPreset {
    /// Look up this backend preset's static definition.
    pub fn def(&self) -> &'static BackendPresetDef {
        let id = self.id();
        BACKEND_PRESET_CATALOG
            .iter()
            .find(|d| d.id == id)
            .expect("every BackendPreset has a catalog entry")
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Test => "test",
        }
    }

    pub fn label(&self) -> &'static str {
        self.def().label
    }

    /// Whether this preset is a user-configurable service (the only one users
    /// ever add). False for the internal test preset.
    pub fn is_user_supplied(&self) -> bool {
        matches!(self, Self::OpenAiCompatible)
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "openai-compatible" => Some(Self::OpenAiCompatible),
            "test" => Some(Self::Test),
            _ => None,
        }
    }

    /// Sensible default model name for first-time use.
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "gpt-4o",
            Self::Test => "test",
        }
    }
}

/// A named, user-configured provider instance.
///
/// This is the primary unit ri-agent uses for provider selection and dispatch.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProviderInstance {
    /// Stable identifier and user-visible name.
    /// Used as the key in config and selection state.
    pub id: String,
    /// The backend preset this instance connects to.
    #[serde(rename = "service_type", alias = "backend_preset")]
    pub backend_preset: BackendPreset,
    /// The API protocol ri-agent uses to talk to this instance.
    pub api_type: ApiType,
    /// Base URL (required for self-hosted services).
    pub base_url: Option<String>,
    /// API key or bearer token, if needed by this service.
    pub api_key: Option<String>,
    /// Last-selected model for this instance.
    pub model: Option<String>,
    /// Sampling temperature (0.0–2.0). `None` defers to the endpoint default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Maximum output tokens the model may emit per turn. `None` defers to the
    /// endpoint default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Optional JSON Schema constraining the model's final answer
    /// (structured output). Fed to rig's `CompletionRequest::output_schema`.
    /// Mutually intended for JSON-only answers — see README caveat: it may
    /// suppress tool calls on providers that reject output_schema + tools in
    /// the same request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

impl ProviderInstance {
    /// Construct a new instance with the recommended defaults for the given
    /// backend preset.
    pub fn new(id: impl Into<String>, backend_preset: BackendPreset) -> Self {
        let api_type = backend_preset.def().default_api.clone();
        Self {
            id: id.into(),
            backend_preset,
            api_type,
            base_url: None,
            api_key: None,
            model: None,
            temperature: None,
            max_tokens: None,
            output_schema: None,
        }
    }

    /// Display label shown in provider selection lists.
    pub fn label(&self) -> String {
        format!("{} ({})", self.id, self.backend_preset.label())
    }

    /// Effective model: last-selected model, or the service default.
    pub fn effective_model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or_else(|| self.backend_preset.default_model())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_preset_has_catalog_entry() {
        let types = [BackendPreset::OpenAiCompatible, BackendPreset::Test];
        for st in &types {
            // Each preset has exactly one usable API type (its default).
            let _ = st.def().default_api;
        }
    }

    #[test]
    fn api_type_round_trips_through_serde() {
        for (serialized, api) in [
            ("openai-responses", ApiType::OpenAiResponses),
            ("openai-compatible", ApiType::OpenAiCompatible),
            ("test", ApiType::Test),
        ] {
            let roundtripped: ApiType = serde_json::from_str(&format!("\"{serialized}\"")).unwrap();
            assert_eq!(roundtripped, api, "parse {serialized}");
            assert_eq!(
                serde_json::to_string(&api).unwrap(),
                format!("\"{serialized}\""),
                "serialize {api:?}"
            );
        }
    }

    #[test]
    fn backend_preset_round_trips_through_id() {
        let types = [BackendPreset::OpenAiCompatible, BackendPreset::Test];
        for st in &types {
            let id = st.id();
            let roundtripped = BackendPreset::from_id(id).unwrap_or_else(|| {
                panic!("from_id failed for id={id}");
            });
            assert_eq!(st.id(), roundtripped.id());
        }
    }

    #[test]
    fn provider_preset_metadata_matches_spec_semantics() {
        // Only the user-facing preset is selectable/user-supplied; test is internal.
        assert!(BackendPreset::OpenAiCompatible.is_user_supplied());
        assert!(!BackendPreset::Test.is_user_supplied());
        assert!(
            BackendPreset::OpenAiCompatible
                .def()
                .url_normalization
                .is_some()
        );
        assert!(BackendPreset::Test.def().url_normalization.is_none());
    }

    #[test]
    fn provider_instance_new_uses_default_api() {
        let inst = ProviderInstance::new("my-endpoint", BackendPreset::OpenAiCompatible);
        assert_eq!(inst.api_type, ApiType::OpenAiCompatible);
        assert_eq!(inst.effective_model(), "gpt-4o");
    }

    #[test]
    fn provider_instance_effective_model_uses_override() {
        let mut inst = ProviderInstance::new("my-endpoint", BackendPreset::OpenAiCompatible);
        inst.model = Some("gpt-5".to_string());
        assert_eq!(inst.effective_model(), "gpt-5");
    }

    #[test]
    fn provider_instance_completion_options_default_to_none() {
        let inst = ProviderInstance::new("my-endpoint", BackendPreset::OpenAiCompatible);
        assert_eq!(inst.temperature, None);
        assert_eq!(inst.max_tokens, None);
        assert_eq!(inst.output_schema, None);
    }

    #[test]
    fn provider_instance_completion_options_round_trip_toml() {
        let mut inst = ProviderInstance::new("my-endpoint", BackendPreset::OpenAiCompatible);
        inst.temperature = Some(0.5);
        inst.max_tokens = Some(128);
        inst.output_schema = Some(serde_json::json!({
            "type": "object",
            "title": "T"
        }));
        let toml = toml::to_string(&inst).unwrap();
        let back: ProviderInstance = toml::from_str(&toml).unwrap();
        assert_eq!(back.temperature, Some(0.5));
        assert_eq!(back.max_tokens, Some(128));
        assert_eq!(
            back.output_schema,
            Some(serde_json::json!({"type": "object", "title": "T"}))
        );
    }

    #[test]
    fn provider_instance_deserializes_without_completion_options() {
        // Old configs without the new keys must keep defaulting to None.
        let text = r#"id = "x"
service_type = "openai-compatible"
api_type = "openai-compatible"
base_url = "http://localhost:1"
api_key = "sk"
"#;
        let inst: ProviderInstance = toml::from_str(text).unwrap();
        assert_eq!(inst.temperature, None);
        assert_eq!(inst.max_tokens, None);
        assert_eq!(inst.output_schema, None);
    }

    #[test]
    fn url_normalization_adds_scheme() {
        let norm = UrlNormalization {
            default_scheme: "https",
            endpoint_label: "URL: ",
        };
        assert_eq!(
            norm.normalize("localhost:8000/v1"),
            Some("https://localhost:8000/v1".into())
        );
        assert_eq!(norm.normalize(""), None);
    }

    #[test]
    fn url_normalization_appends_v1_when_path_is_empty() {
        let norm = UrlNormalization {
            default_scheme: "https",
            endpoint_label: "URL: ",
        };
        assert_eq!(
            norm.normalize("api.openai.com"),
            Some("https://api.openai.com/v1".into())
        );
        // Existing paths (e.g. Open WebUI /api) are left untouched.
        assert_eq!(
            norm.normalize("https://my-webui.example.com/api"),
            Some("https://my-webui.example.com/api".into())
        );
    }

    #[test]
    fn ensure_v1_prefix_only_touches_pathless_urls() {
        assert_eq!(
            ensure_v1_prefix("https://api.openai.com"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            ensure_v1_prefix("http://localhost:11434"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            ensure_v1_prefix("https://my-webui.example.com/api"),
            "https://my-webui.example.com/api"
        );
        assert_eq!(
            ensure_v1_prefix("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
    }
}
