use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub providers: HashMap<String, ProviderCredentials>,
}

impl Default for AuthFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            providers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ProviderCredentials {
    #[serde(rename = "gemini")]
    Gemini {
        access_token: String,
        refresh_token: String,
        expires_at: i64,
        project_id: String,
    },
}

impl ProviderCredentials {
    /// If `expires_at` exceeds `ms_threshold`, it was stored in milliseconds
    /// (pre-v2 format). Divide by 1000 to convert to seconds.
    pub fn migrate_expires_at_ms_to_secs(&mut self, ms_threshold: i64) {
        match self {
            Self::Gemini { expires_at, .. } => {
                if *expires_at > ms_threshold {
                    *expires_at /= 1000;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct GeminiCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub project_id: String,
}

#[cfg(test)]
mod tests {
    use super::{AuthFile, ProviderCredentials};

    #[test]
    fn auth_file_defaults_when_fields_missing() {
        let parsed: AuthFile = serde_json::from_str("{}").expect("parse auth file");
        assert_eq!(parsed.version, 1);
        assert!(parsed.providers.is_empty());
    }

    #[test]
    fn provider_credentials_round_trip_json() {
        let mut auth = AuthFile::default();
        auth.providers.insert(
            "gemini".to_string(),
            ProviderCredentials::Gemini {
                access_token: "gem_tok".to_string(),
                refresh_token: "gem_ref".to_string(),
                expires_at: 333,
                project_id: "proj-123".to_string(),
            },
        );

        let json = serde_json::to_string(&auth).expect("serialize");
        let round_trip: AuthFile = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(round_trip.version, 1);
        assert_eq!(round_trip.providers.len(), 1);
        assert!(matches!(
            round_trip.providers.get("gemini"),
            Some(ProviderCredentials::Gemini { project_id, .. }) if project_id == "proj-123"
        ));
    }
}
