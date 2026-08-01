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

/// Whether a preset represents a user-supplied service or an internal helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendClass {
    UserSuppliedService,
    Internal,
}

/// How the user authenticates a provider instance for a preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    None,
}

/// Whether the endpoint is supplied by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointBehavior {
    UserSupplied,
    Internal,
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
        Some(url.to_string().trim_end_matches('/').to_string())
    }
}

/// Metadata ri-agent keeps about a backend preset.
pub struct BackendPresetDef {
    /// Machine-readable id (matches `BackendPreset` serialisation).
    pub id: &'static str,
    /// Human-readable display label.
    pub label: &'static str,
    /// Which class of backend this preset belongs to.
    pub backend_class: BackendClass,
    /// Whether the user is asked to choose an API type when adding this preset
    /// (i.e. the preset supports more than one selectable protocol).
    pub user_selects_api: bool,
    /// API types the user may pick from for this preset, in picker order.
    pub allowed_apis: &'static [ApiType],
    /// The recommended / default API type.
    pub default_api: ApiType,
    /// Whether the endpoint is predetermined or user-supplied.
    pub endpoint_behavior: EndpointBehavior,
    /// Which authentication mode this preset requires.
    pub auth_mode: AuthMode,
    /// URL normalization parameters for presets that accept a user-supplied URL.
    /// `None` for presets with internal endpoints.
    pub url_normalization: Option<UrlNormalization>,
}

/// Static catalog of all supported backend presets.
pub const BACKEND_PRESET_CATALOG: &[BackendPresetDef] = &[
    BackendPresetDef {
        id: "openai-compatible",
        label: "OpenAI-compatible endpoint",
        backend_class: BackendClass::UserSuppliedService,
        user_selects_api: true,
        allowed_apis: &[ApiType::OpenAiResponses, ApiType::OpenAiCompatible],
        default_api: ApiType::OpenAiCompatible,
        endpoint_behavior: EndpointBehavior::UserSupplied,
        auth_mode: AuthMode::ApiKey,
        url_normalization: Some(UrlNormalization {
            default_scheme: "https",
            endpoint_label: "URL: ",
        }),
    },
    BackendPresetDef {
        id: "test",
        label: "Test (UI exercise)",
        backend_class: BackendClass::Internal,
        user_selects_api: false,
        allowed_apis: &[ApiType::Test],
        default_api: ApiType::Test,
        endpoint_behavior: EndpointBehavior::Internal,
        auth_mode: AuthMode::None,
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

    /// Built-in hosted providers that always appear in the provider picker.
    ///
    /// There are no unconditional hosted singletons anymore — every usable
    /// provider is a user-configured service.
    pub fn built_in_hosted() -> &'static [BackendPreset] {
        &[]
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
    fn built_in_hosted_is_empty() {
        assert!(BackendPreset::built_in_hosted().is_empty());
    }

    #[test]
    fn provider_preset_metadata_matches_spec_semantics() {
        let compat = BackendPreset::OpenAiCompatible.def();
        assert_eq!(compat.backend_class, BackendClass::UserSuppliedService);
        assert_eq!(compat.auth_mode, AuthMode::ApiKey);
        assert_eq!(compat.endpoint_behavior, EndpointBehavior::UserSupplied);
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
}
