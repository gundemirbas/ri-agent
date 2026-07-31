//! `OAuthBackend` trait and concrete implementations for each provider.
//!
//! The trait abstracts the two operations that touch external OAuth servers:
//! - [`OAuthBackend::login`] — run the full interactive login flow and return
//!   new credentials, emitting [`LoginEvent`]s as the flow progresses.
//! - [`OAuthBackend::refresh`] — exchange a stored refresh token for a fresh
//!   access token.
//!
//! Callers in `auth/mod.rs` accept `Arc<dyn OAuthBackend>` so that tests can
//! substitute a [`MockOAuthBackend`](crate::auth::mock::MockOAuthBackend)
//! without making any real HTTP requests.
//!
//! The factory [`real_backend_for`] maps a provider name string to the
//! appropriate real backend. Call sites in `App` use this to construct the
//! backend before spawning the auth task.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, atomic::AtomicBool},
};

use crate::auth::{AuthFlow, LoginEvent, types::ProviderCredentials};

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Boxed future returned by trait methods — required for dyn compatibility.
/// `async fn` in traits (RPITIT) is not dyn-compatible; `Pin<Box<dyn Future>>`
/// is.
pub type BackendFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'static>>;

/// A pluggable OAuth backend for a single provider.
///
/// Implementors handle all provider-specific HTTP; callers work only with the
/// unified [`LoginEvent`] / [`ProviderCredentials`] types.
///
/// Uses `Pin<Box<dyn Future>>` returns to remain dyn-compatible, so callers
/// can hold `Arc<dyn OAuthBackend>` and swap in a mock at test time.
pub trait OAuthBackend: Send + Sync {
    /// Run the full interactive login flow.
    ///
    /// Implementations must call `on_event` with:
    /// - [`LoginEvent::AuthCode`] once the browser URL (and optional device
    ///   code) is known, so the UI can prompt the user.
    /// - [`LoginEvent::Info`] for progress messages during polling.
    ///
    /// The `cancel` flag is checked between polling iterations; when set,
    /// the implementation should bail with an error containing "cancel".
    fn login(
        &self,
        on_event: Box<dyn Fn(LoginEvent) + Send + Sync>,
        cancel: Arc<AtomicBool>,
    ) -> BackendFuture<ProviderCredentials>;

    /// Exchange `refresh_token` for a fresh set of credentials.
    ///
    /// The implementation is responsible for preserving any fields not
    /// returned by the server (e.g. `project_id` for Gemini).
    fn refresh(&self, refresh_token: &str) -> BackendFuture<ProviderCredentials>;
}

// ── GeminiBackend ─────────────────────────────────────────────────────────────

/// Real OAuth backend for Google Gemini (PKCE redirect + Cloud Code project).
///
/// Holds `project_id` so `refresh` can carry it forward without an extra
/// trait parameter — the server does not re-issue it on token refresh.
pub struct GeminiBackend {
    /// Google Cloud project ID discovered during login.
    /// Preserved across refreshes.
    pub project_id: String,
    /// Override token endpoint URL for wiremock testing.
    #[allow(dead_code)]
    pub token_url_override: Option<String>,
}

impl OAuthBackend for GeminiBackend {
    fn login(
        &self,
        on_event: Box<dyn Fn(LoginEvent) + Send + Sync>,
        cancel: Arc<AtomicBool>,
    ) -> BackendFuture<ProviderCredentials> {
        Box::pin(async move {
            let creds = super::gemini::login(
                |ev| match ev {
                    super::gemini::GeminiLoginEvent::OpenBrowser(url) => {
                        on_event(LoginEvent::AuthCode {
                            url,
                            code: None,
                            flow: AuthFlow::RedirectCallback,
                        });
                    }
                    super::gemini::GeminiLoginEvent::Progress(msg) => {
                        on_event(LoginEvent::Info(msg));
                    }
                },
                cancel,
            )
            .await?;
            Ok(ProviderCredentials::Gemini {
                access_token: creds.access_token,
                refresh_token: creds.refresh_token,
                expires_at: creds.expires_at,
                project_id: creds.project_id,
            })
        })
    }

    fn refresh(&self, refresh_token: &str) -> BackendFuture<ProviderCredentials> {
        let refresh_token = refresh_token.to_string();
        let project_id = self.project_id.clone();
        let token_url_override = self.token_url_override.clone();
        Box::pin(async move {
            let creds =
                super::gemini::refresh(&refresh_token, &project_id, token_url_override.as_deref())
                    .await?;
            Ok(ProviderCredentials::Gemini {
                access_token: creds.access_token,
                refresh_token: creds.refresh_token,
                expires_at: creds.expires_at,
                project_id: creds.project_id,
            })
        })
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Construct the real [`OAuthBackend`] for the given provider name.
///
/// For Gemini, `project_id` is loaded from the auth store so `refresh` can
/// carry it forward. Returns `Err` if the provider is unknown, if required
/// environment variables are absent (Gemini), or if stored credentials are
/// needed but absent.
///
/// Call sites in `App` use this to build the backend before spawning the auth
/// task, so a missing env var is surfaced as a `LoginEvent::Error` rather than
/// a panic.
pub fn real_backend_for(provider: &str) -> anyhow::Result<Arc<dyn OAuthBackend>> {
    match provider {
        "gemini" => {
            // Validate env vars eagerly so we fail with a clear error rather
            // than a lazy function call failing inside the async task.
            super::gemini::google_client_id()
                .map_err(|e| anyhow::anyhow!("Gemini not configured: {e}"))?;
            super::gemini::google_client_secret()
                .map_err(|e| anyhow::anyhow!("Gemini not configured: {e}"))?;
            let project_id = super::AuthStore::load_default()
                .ok()
                .and_then(|s| s.get_gemini())
                .map(|c| c.project_id)
                .unwrap_or_default();
            Ok(Arc::new(GeminiBackend {
                project_id,
                token_url_override: None,
            }) as Arc<dyn OAuthBackend>)
        }
        other => Err(anyhow::anyhow!("Unknown OAuth provider: {other}")),
    }
}
