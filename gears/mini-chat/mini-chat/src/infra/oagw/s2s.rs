//! The gear's S2S `SecurityContext` (from `client_credentials`), used for every
//! OAGW call. Empty until the gear obtains it at start ([`S2sBootstrap`]).

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, ClientCredentialsRequest};
use tokio::time::Instant;
use toolkit_security::SecurityContext;
use tracing::{debug, info};

use crate::config::ClientCredentialsConfig;
use crate::domain::error::{DomainError, DomainResult};

/// Holder of the S2S security context.
#[derive(Debug, Default)]
pub struct S2sContext {
    inner: RwLock<Option<SecurityContext>>,
}

impl S2sContext {
    /// Empty holder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the context.
    pub fn set(&self, ctx: SecurityContext) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = Some(ctx);
    }

    /// The current context.
    ///
    /// # Errors
    /// `Internal` while no context has been set.
    pub fn get(&self) -> DomainResult<SecurityContext> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| DomainError::internal("S2S security context is not available yet"))
    }
}

/// Exchange of the gear's `client_credentials` for its S2S context at start.
///
/// The authn plugin registers itself lazily, so "no plugin" / "unavailable" /
/// internal (types registry not ready) answers are retried every `interval`
/// until `deadline`; a rejected exchange fails at once.
#[derive(Debug, Clone, Copy)]
pub struct S2sBootstrap {
    /// Total time allowed for retries.
    pub deadline: Duration,
    /// Pause between attempts.
    pub interval: Duration,
}

impl Default for S2sBootstrap {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            interval: Duration::from_millis(500),
        }
    }
}

impl S2sBootstrap {
    /// [`Self::run`] with the default timings (about 30 s of retries).
    ///
    /// # Errors
    /// See [`Self::run`].
    pub async fn exchange(
        authn: Arc<dyn AuthNResolverClient>,
        creds: &ClientCredentialsConfig,
    ) -> anyhow::Result<SecurityContext> {
        Self::default().run(authn, creds).await
    }

    /// Exchange `creds` through `authn`.
    ///
    /// # Errors
    /// The credentials were rejected, or the plugin stayed unavailable until
    /// the deadline.
    pub async fn run(
        &self,
        authn: Arc<dyn AuthNResolverClient>,
        creds: &ClientCredentialsConfig,
    ) -> anyhow::Result<SecurityContext> {
        let request = ClientCredentialsRequest {
            client_id: creds.client_id.clone(),
            client_secret: creds.client_secret.clone(),
            scopes: Vec::new(),
        };
        let deadline = Instant::now() + self.deadline;
        loop {
            match authn.exchange_client_credentials(&request).await {
                Ok(res) => {
                    info!(client_id = %creds.client_id, "mini-chat S2S security context obtained");
                    return Ok(res.security_context);
                }
                Err(err) if is_retryable(&err) && Instant::now() < deadline => {
                    debug!(%err, "authn plugin not ready for the client_credentials exchange; retrying");
                    tokio::time::sleep(self.interval).await;
                }
                Err(err) => {
                    return Err(anyhow::anyhow!(
                        "mini-chat client_credentials exchange failed for client '{}': {err}",
                        creds.client_id
                    ));
                }
            }
        }
    }
}

/// Answers of an authn resolver whose plugin is not registered / ready yet.
const fn is_retryable(err: &AuthNResolverError) -> bool {
    matches!(
        err,
        AuthNResolverError::NoPluginAvailable
            | AuthNResolverError::ServiceUnavailable(_)
            | AuthNResolverError::Internal(_)
    )
}

#[cfg(test)]
#[path = "s2s_tests.rs"]
mod s2s_tests;
