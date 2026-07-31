//! Mock OAuth backend for testing login flows.

use std::sync::{Arc, Mutex, atomic::AtomicBool};

use crate::auth::LoginEvent;
use crate::auth::backend::{BackendFuture, OAuthBackend};
use crate::auth::types::ProviderCredentials;

/// A mock [`OAuthBackend`] that returns a pre-configured result from `login`.
pub struct MockOAuthBackend {
    login_result: Mutex<Option<anyhow::Result<ProviderCredentials>>>,
}

impl MockOAuthBackend {
    pub fn new() -> Self {
        Self {
            login_result: Mutex::new(None),
        }
    }

    /// Configure what `login` should return.
    pub fn expect_login(self, result: anyhow::Result<ProviderCredentials>) -> Self {
        *self.login_result.lock().unwrap() = Some(result);
        self
    }
}

impl Default for MockOAuthBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthBackend for MockOAuthBackend {
    fn login(
        &self,
        on_event: Box<dyn Fn(LoginEvent) + Send + Sync>,
        _cancel: Arc<AtomicBool>,
    ) -> BackendFuture<ProviderCredentials> {
        // Emit expected event sequence before returning the result.
        on_event(LoginEvent::Info("Starting login via mock".into()));
        on_event(LoginEvent::AuthCode {
            url: "https://mock.example.com/device".into(),
            code: Some("MOCK-CODE".into()),
            flow: crate::auth::AuthFlow::RedirectCallback,
        });

        let result = self.login_result.lock().unwrap().take();

        Box::pin(async move {
            match result {
                Some(Ok(creds)) => Ok(creds),
                Some(Err(e)) => Err(e),
                None => Err(anyhow::anyhow!(
                    "MockOAuthBackend: no login result configured"
                )),
            }
        })
    }

    fn refresh(&self, _refresh_token: &str) -> BackendFuture<ProviderCredentials> {
        Box::pin(async move { Err(anyhow::anyhow!("MockOAuthBackend: refresh not implemented")) })
    }
}

/// Create fake Gemini credentials for testing.
pub fn fake_gemini_creds() -> ProviderCredentials {
    ProviderCredentials::Gemini {
        access_token: "fake-gemini-token".to_string(),
        refresh_token: "fake-refresh-token".to_string(),
        expires_at: 9999999999i64,
        project_id: "fake-project".to_string(),
    }
}
