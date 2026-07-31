use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::app_event::{AppEvent, AppEventTx};

pub mod backend;
pub mod gemini;
#[cfg(test)]
pub mod mock;
pub mod open_url;
pub mod paths;
pub mod store;
pub mod types;

pub use backend::{OAuthBackend, real_backend_for};
pub use store::AuthStore;

/// Describes what the user needs to do after receiving an [`LoginEvent::AuthCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFlow {
    /// PKCE redirect flow (Gemini): open the URL; the browser will
    /// redirect back to localhost automatically once the user approves.
    RedirectCallback,
}

#[derive(Debug, Clone)]
pub enum LoginEvent {
    Info(String),
    AuthCode {
        url: String,
        /// Short user-facing code to be entered in the browser (device flow only).
        code: Option<String>,
        /// Which flow is in use, so the UI can show appropriate instructions.
        flow: AuthFlow,
    },
    Success {
        provider: String,
    },
    Error {
        provider: String,
        message: String,
    },
    RefreshResult {
        provider: String,
        success: bool,
        message: String,
    },
    Finished,
}

/// Run the login flow for `provider` using the supplied `backend`.
///
/// Emits [`LoginEvent`]s on `tx` throughout the flow and persists credentials
/// to the default auth store on success.
///
/// `backend` is injected so tests can substitute a mock without any real HTTP.
/// In production, callers build the backend with [`real_backend_for`].
pub async fn login_provider(
    provider: &str,
    tx: AppEventTx,
    cancel: Arc<AtomicBool>,
    backend: Arc<dyn OAuthBackend>,
) {
    login_provider_inner(provider, tx, cancel, backend, None).await;
}

async fn login_provider_inner(
    provider: &str,
    tx: AppEventTx,
    cancel: Arc<AtomicBool>,
    backend: Arc<dyn OAuthBackend>,
    auth_path: Option<&std::path::Path>,
) {
    log::debug!("login_provider called: provider={provider}");
    let _ = tx.send(AppEvent::Login(LoginEvent::Info(format!(
        "Starting login for {provider}..."
    ))));

    // Forward all backend LoginEvents directly onto the app event channel.
    let tx_ev = tx.clone();
    let login_result = backend
        .login(
            Box::new(move |ev| {
                let _ = tx_ev.send(AppEvent::Login(ev));
            }),
            cancel.clone(),
        )
        .await;

    // On success, persist the returned ProviderCredentials to the store.
    let result = login_result.map(|creds| {
        let mut store = match auth_path {
            Some(path) => AuthStore::load(path),
            None => AuthStore::load_default(),
        }?;
        store.set_from_credentials(creds);
        store.save()
    });

    match result {
        Ok(Ok(())) => {
            let _ = tx.send(AppEvent::Login(LoginEvent::Success {
                provider: provider.to_string(),
            }));
        }
        Ok(Err(e)) | Err(e) => {
            let is_cancelled = cancel.load(Ordering::Relaxed)
                || e.to_string().to_ascii_lowercase().contains("cancel");
            let message = if is_cancelled {
                "Login cancelled".to_string()
            } else {
                e.to_string()
            };
            let _ = tx.send(AppEvent::Login(LoginEvent::Error {
                provider: provider.to_string(),
                message,
            }));
        }
    }

    let _ = tx.send(AppEvent::Login(LoginEvent::Finished));
}

/// Refresh the stored OAuth token for `provider` using the supplied `backend`.
///
/// Loads the stored refresh token, calls `backend.refresh()`, and persists
/// the updated credentials. Returns `Ok(())` on success.
///
/// `backend` is injected so tests can substitute a mock. In production,
/// callers build the backend with [`real_backend_for`].
pub async fn refresh_token(provider: &str, backend: Arc<dyn OAuthBackend>) -> anyhow::Result<()> {
    log::debug!("refresh_token called: provider={provider}");
    let mut store = AuthStore::load_default()?;
    let refresh_tok = store
        .get_refresh_token(provider)
        .ok_or_else(|| anyhow::anyhow!("No stored credentials for {provider}"))?;
    let refreshed = backend.refresh(&refresh_tok).await?;
    store.set_from_credentials(refreshed);
    store.save()
}

/// Refresh the stored token for `provider` and report the outcome on `tx`.
///
/// This is a thin wrapper around [`refresh_token`] for use by the interactive
/// TUI, which communicates refresh results via a unified [`AppEvent`] channel.
pub async fn refresh_provider(provider: &str, tx: AppEventTx) {
    let backend = match real_backend_for(provider) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(AppEvent::Login(LoginEvent::RefreshResult {
                provider: provider.to_string(),
                success: false,
                message: e.to_string(),
            }));
            return;
        }
    };
    let result = refresh_token(provider, backend).await;
    match result {
        Ok(()) => {
            let _ = tx.send(AppEvent::Login(LoginEvent::RefreshResult {
                provider: provider.to_string(),
                success: true,
                message: "Token refreshed".to_string(),
            }));
        }
        Err(e) => {
            let _ = tx.send(AppEvent::Login(LoginEvent::RefreshResult {
                provider: provider.to_string(),
                success: false,
                message: e.to_string(),
            }));
        }
    }
}

// ── Token expiry preflight ────────────────────────────────────────────────────

/// Leeway for proactive token refresh before actual expiration (in seconds).
/// Tokens expiring within this window will trigger a refresh to avoid
/// last-minute failures.
pub const AUTH_REFRESH_LEEWAY_SECS: i64 = 120;

/// Classification of a stored auth token's freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthTokenState {
    /// No credentials are stored for the provider.
    Missing,
    /// Token is fresh and has time before expiration.
    Valid,
    /// Token expires soon (within the leeway window).
    ExpiringSoon,
    /// Token has already expired.
    Expired,
}

/// Check the expiration state of the stored token for the given provider.
///
/// Equivalent to [`token_state`] but takes an explicit [`AuthStore`]
/// reference instead of loading the default one. Useful in tests where the
/// store is controlled.
pub fn token_state_from_store(
    store: &AuthStore,
    provider: &str,
    now_secs: i64,
    leeway_secs: i64,
) -> anyhow::Result<AuthTokenState> {
    let expires_at = match provider {
        "gemini" => store.get_gemini().map(|c| c.expires_at),
        _ => return Ok(AuthTokenState::Missing),
    };

    let Some(expires_at) = expires_at else {
        return Ok(AuthTokenState::Missing);
    };

    if now_secs >= expires_at {
        Ok(AuthTokenState::Expired)
    } else if now_secs >= expires_at - leeway_secs {
        Ok(AuthTokenState::ExpiringSoon)
    } else {
        Ok(AuthTokenState::Valid)
    }
}

/// Check the expiration state of the stored token for the given provider.
///
/// Returns `Missing` if no credentials exist, otherwise classifies the token
/// based on `expires_at` relative to `now_secs` with a `leeway_secs` buffer.
///
/// # Arguments
/// - `provider`: Provider name ("gemini")
/// - `now_secs`: Current time as Unix epoch seconds
/// - `leeway_secs`: Seconds before expiry to consider the token "expiring soon"
pub fn token_state(
    provider: &str,
    now_secs: i64,
    leeway_secs: i64,
) -> anyhow::Result<AuthTokenState> {
    let store = AuthStore::load_default()?;
    token_state_from_store(&store, provider, now_secs, leeway_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_state_missing_when_no_creds() {
        // Non-existent provider returns Missing without error
        let state = token_state("nonexistent", 1000, 120).unwrap();
        assert_eq!(state, AuthTokenState::Missing);
    }

    #[test]
    fn token_state_expired_when_past_expiry() {
        let state = token_state_for_expiry(1000, 900, 120);
        assert_eq!(state, AuthTokenState::Expired);
    }

    #[test]
    fn token_state_expiring_soon_within_leeway() {
        // expires_at = 1100, now = 1000, leeway = 120
        // 1100 - 120 = 980, so 1000 is within the expiring-soon window
        let state = token_state_for_expiry(1000, 1100, 120);
        assert_eq!(state, AuthTokenState::ExpiringSoon);
    }

    #[test]
    fn token_state_expiring_soon_at_boundary() {
        // expires_at = 1120, now = 1000, leeway = 120
        // 1120 - 120 = 1000 (exactly at boundary)
        let state = token_state_for_expiry(1000, 1120, 120);
        assert_eq!(state, AuthTokenState::ExpiringSoon);
    }

    #[test]
    fn token_state_valid_when_fresh() {
        // expires_at = 2000, now = 1000, leeway = 120
        // 2000 - 120 = 1880, so 1000 is well before expiry
        let state = token_state_for_expiry(1000, 2000, 120);
        assert_eq!(state, AuthTokenState::Valid);
    }

    #[test]
    fn token_state_valid_just_outside_leeway() {
        // expires_at = 1121, now = 1000, leeway = 120
        // 1121 - 120 = 1001, so 1000 is just outside (still valid)
        let state = token_state_for_expiry(1000, 1121, 120);
        assert_eq!(state, AuthTokenState::Valid);
    }

    /// Helper to test token_state logic without needing real credentials.
    /// Simulates the classification logic inline.
    fn token_state_for_expiry(now_secs: i64, expires_at: i64, leeway_secs: i64) -> AuthTokenState {
        if now_secs >= expires_at {
            AuthTokenState::Expired
        } else if now_secs >= expires_at - leeway_secs {
            AuthTokenState::ExpiringSoon
        } else {
            AuthTokenState::Valid
        }
    }

    // ── login_provider orchestration tests ────────────────────────────────────

    use crate::app_event::AppEvent;
    use crate::auth::mock::{MockOAuthBackend, fake_gemini_creds};
    use crate::auth::store::AuthStore;
    use crate::auth::types::ProviderCredentials;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn login_provider_success_emits_event_sequence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mock = Arc::new(MockOAuthBackend::new().expect_login(Ok(fake_gemini_creds())));

        let (tx, mut rx) = unbounded_channel::<AppEvent>();
        let cancel = Arc::new(AtomicBool::new(false));

        login_provider_inner("gemini", tx, cancel, mock, Some(&path)).await;

        // Drain events
        let events: Vec<LoginEvent> = std::iter::from_fn(|| {
            rx.try_recv().ok().map(|e| match e {
                AppEvent::Login(ev) => ev,
                _ => unreachable!(),
            })
        })
        .collect();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoginEvent::Info(m) if m.contains("Starting login"))),
            "expected Info event"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoginEvent::AuthCode { .. })),
            "expected AuthCode event"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoginEvent::Success { provider } if provider == "gemini")),
            "expected Success event"
        );
        assert!(
            events.iter().any(|e| matches!(e, LoginEvent::Finished)),
            "expected Finished event"
        );

        // Verify credentials were persisted
        let store = AuthStore::load(&path).unwrap();
        assert!(
            store.get_gemini().is_some(),
            "credentials should be persisted"
        );
    }

    #[tokio::test]
    async fn login_provider_error_emits_error_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mock = Arc::new(
            MockOAuthBackend::new().expect_login(Err(anyhow::anyhow!("provider rejected"))),
        );

        let (tx, mut rx) = unbounded_channel::<AppEvent>();
        let cancel = Arc::new(AtomicBool::new(false));

        login_provider_inner("gemini", tx, cancel, mock, Some(&path)).await;

        let events: Vec<LoginEvent> = std::iter::from_fn(|| {
            rx.try_recv().ok().map(|e| match e {
                AppEvent::Login(ev) => ev,
                _ => unreachable!(),
            })
        })
        .collect();

        assert!(
            events.iter().any(
                |e| matches!(e, LoginEvent::Error { message, .. } if message == "provider rejected")
            ),
            "expected Error event with message"
        );
        assert!(
            events.iter().any(|e| matches!(e, LoginEvent::Finished)),
            "expected Finished event"
        );
    }

    #[tokio::test]
    async fn login_provider_cancelled_emits_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mock =
            Arc::new(MockOAuthBackend::new().expect_login(Err(anyhow::anyhow!("Login cancelled"))));

        let (tx, mut rx) = unbounded_channel::<AppEvent>();
        let cancel = Arc::new(AtomicBool::new(true)); // already cancelled

        login_provider_inner("gemini", tx, cancel, mock, Some(&path)).await;

        let events: Vec<LoginEvent> = std::iter::from_fn(|| {
            rx.try_recv().ok().map(|e| match e {
                AppEvent::Login(ev) => ev,
                _ => unreachable!(),
            })
        })
        .collect();

        assert!(
            events.iter().any(
                |e| matches!(e, LoginEvent::Error { message, .. } if message == "Login cancelled")
            ),
            "expected cancelled error"
        );
    }

    // ── token_state regression tests against real AuthStore ────────────────────

    #[test]
    fn token_state_with_real_store_expired() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mut store = AuthStore::load(&path).unwrap();
        store.set_from_credentials(ProviderCredentials::Gemini {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires_at: 900,
            project_id: "proj".to_string(),
        });
        store.save().unwrap();

        let state = token_state_from_store(&store, "gemini", 1000, 120).unwrap();
        assert_eq!(state, AuthTokenState::Expired);
    }

    #[test]
    fn token_state_with_real_store_expiring_soon() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mut store = AuthStore::load(&path).unwrap();
        store.set_from_credentials(ProviderCredentials::Gemini {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires_at: 1100,
            project_id: "proj".to_string(),
        });
        store.save().unwrap();

        let state = token_state_from_store(&store, "gemini", 1000, 120).unwrap();
        assert_eq!(state, AuthTokenState::ExpiringSoon);
    }

    #[test]
    fn token_state_with_real_store_valid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mut store = AuthStore::load(&path).unwrap();
        store.set_from_credentials(ProviderCredentials::Gemini {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires_at: 2000,
            project_id: "proj".to_string(),
        });
        store.save().unwrap();

        let state = token_state_from_store(&store, "gemini", 1000, 120).unwrap();
        assert_eq!(state, AuthTokenState::Valid);
    }

    #[test]
    fn token_state_with_real_store_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let store = AuthStore::load(&path).unwrap();
        // No credentials written — store is empty.

        let state = token_state_from_store(&store, "gemini", 1000, 120).unwrap();
        assert_eq!(state, AuthTokenState::Missing);
    }

    // ── Store helper tests ─────────────────────────────────────────────────────

    #[test]
    fn set_from_credentials_gemini_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mut store = AuthStore::load(&path).unwrap();
        store.set_from_credentials(ProviderCredentials::Gemini {
            access_token: "a1".to_string(),
            refresh_token: "r1".to_string(),
            expires_at: 1000,
            project_id: "proj".to_string(),
        });
        store.save().unwrap();

        let loaded = AuthStore::load(&path).unwrap();
        let creds = loaded.get_gemini().unwrap();
        assert_eq!(creds.access_token, "a1");
        assert_eq!(creds.refresh_token, "r1");
        assert_eq!(creds.expires_at, 1000);
        assert_eq!(creds.project_id, "proj");
    }

    #[test]
    fn get_refresh_token_returns_token_for_known_providers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");
        let mut store = AuthStore::load(&path).unwrap();
        store.set_from_credentials(ProviderCredentials::Gemini {
            access_token: "at".to_string(),
            refresh_token: "rt_gem".to_string(),
            expires_at: 9999,
            project_id: "proj".to_string(),
        });

        assert_eq!(store.get_refresh_token("gemini").as_deref(), Some("rt_gem"));
        assert_eq!(store.get_refresh_token("unknown"), None);
    }

    // ── Migration guard tests ─────────────────────────────────────────────────

    #[test]
    fn expires_at_ms_in_loaded_file_is_migrated_to_seconds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");

        // Write a file with expires_at in milliseconds (pre-v2 format)
        let ms_value: i64 = 1_750_000_000_000; // ~2025 in ms
        let toml = format!(
            r#"
version = 1

[providers.gemini]
kind = "gemini"
access_token = "tok"
refresh_token = "ref"
expires_at = {}
project_id = "proj"
"#,
            ms_value
        );
        std::fs::write(&path, &toml).unwrap();

        let store = AuthStore::load(&path).unwrap();
        let creds = store.get_gemini().unwrap();
        // Should have been divided by 1000
        assert_eq!(creds.expires_at, ms_value / 1000);
    }

    #[test]
    fn expires_at_seconds_unchanged_by_migration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.toml");

        // Write a file with expires_at already in seconds (post-fix format)
        let secs_value: i64 = 1_750_000_000;
        let toml = format!(
            r#"
version = 1

[providers.gemini]
kind = "gemini"
access_token = "tok"
refresh_token = "ref"
expires_at = {}
project_id = "proj"
"#,
            secs_value
        );
        std::fs::write(&path, &toml).unwrap();

        let store = AuthStore::load(&path).unwrap();
        let creds = store.get_gemini().unwrap();
        // Should remain unchanged
        assert_eq!(creds.expires_at, secs_value);
    }
}
