//! S2S security context: obtained once at start from `authn_resolver` (client credentials) and
//! used for every OAGW call the gear makes (spec 4a.3).

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use crate::config::ClientCredentials;
use crate::domain::error::DomainError;

/// Delay between exchange attempts while the authn plugin is not available.
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Holder of the S2S [`SecurityContext`]; empty until `serve` has exchanged the credentials.
/// Cheap to clone (all clones share the slot).
#[derive(Clone, Default)]
pub struct S2sContext(Arc<ArcSwapOption<SecurityContext>>);

impl S2sContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The S2S context.
    ///
    /// # Errors
    /// `ProviderUnavailable` while the context has not been established yet.
    pub fn get(&self) -> Result<SecurityContext, DomainError> {
        self.0.load_full().map(|ctx| (*ctx).clone()).ok_or_else(|| {
            DomainError::ProviderUnavailable("S2S context not established".to_owned())
        })
    }

    pub fn set(&self, ctx: SecurityContext) {
        self.0.store(Some(Arc::new(ctx)));
    }
}

/// Exchanges the client credentials for the S2S context. Retries every second while the authn
/// plugin is not available (`NoPluginAvailable` / `ServiceUnavailable`) until `cancel` fires.
///
/// # Errors
/// `TokenAcquisitionFailed` (and any other error) fails immediately; cancellation while waiting
/// for the plugin also returns an error (the caller checks `cancel.is_cancelled()`).
pub async fn obtain_s2s_context(
    authn: &dyn AuthNResolverClient,
    creds: &ClientCredentials,
    cancel: &CancellationToken,
) -> anyhow::Result<SecurityContext> {
    loop {
        let request = ClientCredentialsRequest {
            client_id: creds.client_id.clone(),
            client_secret: SecretString::from(creds.client_secret.clone()),
            scopes: Vec::new(),
        };
        match authn.exchange_client_credentials(&request).await {
            Ok(AuthenticationResult { security_context }) => return Ok(security_context),
            Err(
                e @ (AuthNResolverError::NoPluginAvailable
                | AuthNResolverError::ServiceUnavailable(_)),
            ) => {
                tracing::debug!(error = %e, "authn plugin not available yet; retrying S2S exchange");
                tokio::select! {
                    () = cancel.cancelled() => anyhow::bail!("cancelled while waiting for the authn plugin"),
                    () = tokio::time::sleep(RETRY_INTERVAL) => {}
                }
            }
            Err(e) => anyhow::bail!("S2S client-credentials exchange failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::authn::{FakeAuthn, S2S_SUBJECT, s2s_security_context};

    fn creds() -> ClientCredentials {
        ClientCredentials {
            client_id: "mini-chat".to_owned(),
            client_secret: "s3cret".to_owned(),
        }
    }

    #[test]
    fn context_is_unavailable_until_set() {
        let s2s = S2sContext::new();
        assert!(matches!(
            s2s.get(),
            Err(DomainError::ProviderUnavailable(_))
        ));
        let clone = s2s.clone();
        s2s.set(s2s_security_context());
        assert_eq!(
            clone.get().unwrap().subject_id(),
            S2S_SUBJECT,
            "clones share the slot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_every_second_while_the_plugin_is_unavailable() {
        let authn = FakeAuthn::new();
        authn.script(vec![
            Err(AuthNResolverError::NoPluginAvailable),
            Err(AuthNResolverError::ServiceUnavailable(
                "starting".to_owned(),
            )),
        ]);
        let started = tokio::time::Instant::now();
        let ctx = obtain_s2s_context(&authn, &creds(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(ctx.subject_id(), S2S_SUBJECT);
        assert_eq!(authn.calls(), 3);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(
            authn.exchanges()[0],
            ("mini-chat".to_owned(), "s3cret".to_owned())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn token_acquisition_failure_is_not_retried() {
        let authn = FakeAuthn::new();
        authn.script(vec![Err(AuthNResolverError::TokenAcquisitionFailed(
            "invalid client credentials".to_owned(),
        ))]);
        let err = obtain_s2s_context(&authn, &creds(), &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid client credentials"), "{err}");
        assert_eq!(authn.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_the_retry_loop() {
        let authn = FakeAuthn::new();
        authn.script(
            (0..1000)
                .map(|_| Err(AuthNResolverError::NoPluginAvailable))
                .collect(),
        );
        let cancel = CancellationToken::new();
        let creds = creds();
        let waiter = obtain_s2s_context(&authn, &creds, &cancel);
        let canceller = async {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            cancel.cancel();
        };
        let (res, ()) = tokio::join!(waiter, canceller);
        assert!(res.is_err());
        assert_eq!(authn.calls(), 3, "attempts at 0, 1 and 2 s");
    }
}
