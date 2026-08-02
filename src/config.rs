use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::Context;

use crate::provider_instance::{BackendPreset, ProviderInstance};

/// Display thresholds — presentation choices that control how much content
/// is shown in the UI. These do not affect how much is sent to the model.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DisplayConfig {
    /// Maximum lines of a shell command shown in the live turn view.
    pub max_shell_command_lines: usize,
    /// Characters before a command label switches to multi-line display.
    pub max_one_line_chars: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            max_shell_command_lines: 5,
            max_one_line_chars: 120,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct RiConfig {
    /// Path to the theme file. Overridden by the `--theme` CLI flag.
    pub theme: Option<PathBuf>,
    /// UI display thresholds.
    #[serde(default)]
    pub display: DisplayConfig,

    /// The id of the currently active provider instance.
    pub provider: Option<String>,
    /// The name of the currently active agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Route agent tool subprocesses through the rootless container sandbox
    /// (user namespace + chroot). Linux-only; overridable via `--sandbox`.
    ///
    /// See docs/CONTAINER-RUNTIME-SPEC.md.
    #[serde(default)]
    pub sandbox: bool,
    pub thinking: Option<String>,
    #[serde(default)]
    pub thinking_by_model: HashMap<String, String>,

    /// Named provider instances.
    #[serde(default)]
    pub providers: Vec<ProviderInstance>,
}

impl RiConfig {
    /// Return a reference to the provider instance with the given id, if any.
    pub fn find_provider(&self, id: &str) -> Option<&ProviderInstance> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Return a mutable reference to the provider instance with the given id.
    pub fn find_provider_mut(&mut self, id: &str) -> Option<&mut ProviderInstance> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    /// Add or replace a provider instance (keyed by id).
    pub fn upsert_provider(&mut self, instance: ProviderInstance) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.id == instance.id) {
            *existing = instance;
        } else {
            self.providers.push(instance);
        }
    }

    /// Remove a provider instance by id. Returns `true` if it was present.
    pub fn remove_provider(&mut self, id: &str) -> bool {
        let before = self.providers.len();
        self.providers.retain(|p| p.id != id);
        self.providers.len() < before
    }

    /// Return all user-configured provider instances, sorted by id.
    ///
    /// There are no built-in hosted providers — every usable provider is a
    /// user-configured instance (the internal test preset is excluded).
    pub fn resolve_effective_providers(&self) -> Vec<ProviderInstance> {
        let mut result: Vec<ProviderInstance> = self
            .providers
            .iter()
            .filter(|p| p.backend_preset.is_user_supplied())
            .cloned()
            .collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        result
    }

    /// Resolve a provider instance by id, falling back to the built-in catalog
    /// when no config entry exists.
    pub fn resolve_provider(&self, id: &str) -> Option<ProviderInstance> {
        if let Some(inst) = self.find_provider(id) {
            return Some(inst.clone());
        }
        BackendPreset::from_id(id).map(|preset| ProviderInstance::new(id, preset))
    }
}

impl RiConfig {
    /// Load from $XDG_CONFIG_HOME/ri/config.toml (or ~/.config/ri/config.toml).
    /// Missing file is not an error and returns `Default`.
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        Self::from_toml_str(&raw)
            .with_context(|| format!("Failed to parse TOML config file: {}", path.display()))
    }

    pub fn from_toml_str(raw: &str) -> anyhow::Result<Self> {
        match toml::from_str(raw) {
            Ok(cfg) => Ok(cfg),
            Err(e) => {
                // Detect the unsupported `[providers.<id>]` table form and
                // point users at the `[[providers]]` array format before the
                // generic TOML error scrolls past. `toml` rejects a table for
                // the `Vec<ProviderInstance>` field, which on its own reads
                // like "invalid type: map, expected a sequence" — useless
                // without knowing what to fix.
                if raw.contains("[providers.") {
                    Err(anyhow::anyhow!(
                        "{e}\n\n\
                         Found the unsupported `[providers.<id>]` table form. Provider instances \
                         must be declared as a `[[providers]]` array with an `id` field, e.g.:\n\
                         [[providers]]\n\
                         id = \"my-endpoint\"\n\
                         service_type = \"openai-compatible\"\n\
                         api_type = \"openai-compatible\"\n\
                         base_url = \"https://...\"\n\
                         api_key = \"...\"\n\
                         See docs/PROVIDER-MODEL-SPEC.md for the full format."
                    ))
                } else {
                    Err(anyhow::Error::new(e))
                }
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path()?;
        save_config(&path, self)
    }
}

fn save_config(path: &std::path::Path, config: &RiConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let body = toml::to_string_pretty(config)?;
    crate::atomic_file::save_atomic(path, &body)
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(crate::dirs::project_dirs()?
        .config_dir()
        .join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::{RiConfig, save_config};
    use crate::provider_instance::{ApiType, BackendPreset, ProviderInstance};

    // ── Instance-format config tests ─────────────────────────────────────────

    #[test]
    fn save_config_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let cfg = RiConfig::default();

        save_config(&path, &cfg).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn legacy_provider_sections_do_not_synthesise_instances() {
        let raw = r#"
provider = "gemini"

[openai]
api_key = "sk-test"
model = "gpt-4o-mini"

[gemini]
model = "gemini-2.5-pro"

[ollama]
base_url = "http://gpu-box:11434"
model = "llama3.1"

[open_webui]
base_url = "https://my-webui.example.com"
api_key = "token123"
model = "llama3.1"
"#;
        let cfg = RiConfig::from_toml_str(raw).unwrap();
        assert!(cfg.providers.is_empty());
        assert!(cfg.find_provider("gemini").is_none());
        assert!(cfg.find_provider("openai").is_none());
        assert!(cfg.find_provider("ollama").is_none());
        assert!(cfg.find_provider("open-webui").is_none());
    }

    #[test]
    fn legacy_provider_table_format_reports_supportive_hint() {
        let raw = r#"
provider = "my-endpoint"

[providers.my-endpoint]
service_type = "openai-compatible"
api_key = "sk-test"
"#;
        let err = RiConfig::from_toml_str(raw).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("[[providers]]"),
            "missing format hint: {rendered}"
        );
        assert!(
            rendered.contains("id = \"my-endpoint\""),
            "missing example id: {rendered}"
        );
    }

    #[test]
    fn other_parse_errors_do_not_mention_providers_array() {
        // A completely unrelated parse error (unknown table) still surfaces the
        // raw toml message without the providers-array hint.
        let raw = "this is not = [valid";
        let err = RiConfig::from_toml_str(raw).unwrap_err();
        assert!(!err.to_string().contains("[[providers]]"));
    }

    #[test]
    fn new_providers_format_parses_directly() {
        let raw = r#"
provider = "work-webui"

[[providers]]
id = "work-webui"
backend_preset = "openai-compatible"
api_type = "openai-compatible"
base_url = "https://work.example.com"
api_key = "tok"
model = "llama3.1"

[[providers]]
id = "gpu-box"
backend_preset = "openai-compatible"
api_type = "openai-compatible"
base_url = "http://gpu-box:11434"
"#;
        let cfg = RiConfig::from_toml_str(raw).unwrap();
        assert_eq!(cfg.providers.len(), 2);

        let webui = cfg.find_provider("work-webui").unwrap();
        assert_eq!(webui.backend_preset, BackendPreset::OpenAiCompatible);
        assert_eq!(webui.base_url.as_deref(), Some("https://work.example.com"));

        let gpu = cfg.find_provider("gpu-box").unwrap();
        assert_eq!(gpu.backend_preset, BackendPreset::OpenAiCompatible);
        assert_eq!(gpu.api_type, ApiType::OpenAiCompatible);

        assert!(cfg.find_provider("openrouter").is_none());

        assert_eq!(cfg.provider.as_deref(), Some("work-webui"));
    }

    #[test]
    fn upsert_and_remove_provider() {
        let mut cfg = RiConfig::default();
        use crate::provider_instance::ProviderInstance;
        let inst = ProviderInstance::new("my-provider", BackendPreset::OpenAiCompatible);
        cfg.upsert_provider(inst);
        assert!(cfg.find_provider("my-provider").is_some());

        // Upsert again with model set
        let mut inst2 = ProviderInstance::new("my-provider", BackendPreset::OpenAiCompatible);
        inst2.model = Some("mistral".into());
        cfg.upsert_provider(inst2);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(
            cfg.find_provider("my-provider").unwrap().model.as_deref(),
            Some("mistral")
        );

        assert!(cfg.remove_provider("my-provider"));
        assert!(cfg.find_provider("my-provider").is_none());
        assert!(!cfg.remove_provider("my-provider")); // idempotent
    }

    #[test]
    fn resolve_effective_providers_empty_on_empty_config() {
        let cfg = RiConfig::default();
        assert!(cfg.resolve_effective_providers().is_empty());
    }

    #[test]
    fn resolve_effective_providers_includes_user_providers() {
        let mut cfg = RiConfig::default();
        cfg.upsert_provider(ProviderInstance::new(
            "my-provider",
            BackendPreset::OpenAiCompatible,
        ));
        let effective = cfg.resolve_effective_providers();
        assert!(effective.iter().any(|p| p.id == "my-provider"));
        // No unconditional hosted singletons anymore.
        assert!(effective.iter().all(|p| p.id != "gemini"));
    }

    #[test]
    fn resolve_effective_providers_prefers_config_override_for_builtin() {
        let mut cfg = RiConfig::default();
        let mut inst = ProviderInstance::new("openai", BackendPreset::OpenAiCompatible);
        inst.model = Some("override-model".to_string());
        cfg.upsert_provider(inst);
        let effective = cfg.resolve_effective_providers();
        let openai = effective.iter().find(|p| p.id == "openai").unwrap();
        assert_eq!(openai.model.as_deref(), Some("override-model"));
    }

    #[test]
    fn upsert_provider_replaces_existing_provider_after_rename_when_old_id_removed() {
        let mut cfg = RiConfig::default();
        let mut original = crate::provider_instance::ProviderInstance::new(
            "gpu-box",
            BackendPreset::OpenAiCompatible,
        );
        original.base_url = Some("http://gpu-box:11434".to_string());
        cfg.upsert_provider(original);

        let mut renamed = crate::provider_instance::ProviderInstance::new(
            "renamed-box",
            BackendPreset::OpenAiCompatible,
        );
        renamed.base_url = Some("http://gpu-box:11434".to_string());

        assert!(cfg.remove_provider("gpu-box"));
        cfg.upsert_provider(renamed.clone());

        assert!(cfg.find_provider("gpu-box").is_none());
        let inst = cfg
            .find_provider("renamed-box")
            .expect("renamed provider present");
        assert_eq!(inst.base_url.as_deref(), Some("http://gpu-box:11434"));
        assert_eq!(cfg.providers.len(), 1);
    }
}
