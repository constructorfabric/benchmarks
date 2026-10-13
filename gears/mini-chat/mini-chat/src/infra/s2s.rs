//! S2S security context used for every OAGW call (S§1.2): exchanged from
//! `client_credentials` via `authn_resolver` at gear start, exchanged lazily
//! on first use when that has not succeeded yet.

use std::sync::{Arc, PoisonError, RwLock};

use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, ClientCredentialsRequest};
use secrecy::SecretString;
use toolkit_security::SecurityContext;

use crate::config::ClientCredentials;

/// Holder of the S2S `SecurityContext`.
pub struct S2sContextProvider {
    cached: RwLock<Option<SecurityContext>>,
    exchanger: Option<(Arc<dyn AuthNResolverClient>, ClientCredentials)>,
}

impl S2sContextProvider {
    /// Exchanges `creds` through `authn` (lazily, cached after success).
    #[must_use]
    pub fn new(authn: Arc<dyn AuthNResolverClient>, creds: ClientCredentials) -> Self {
        Self {
            cached: RwLock::new(None),
            exchanger: Some((authn, creds)),
        }
    }

    /// Always returns `ctx` (tests).
    #[must_use]
    pub fn fixed(ctx: SecurityContext) -> Self {
        Self {
            cached: RwLock::new(Some(ctx)),
            exchanger: None,
        }
    }

    /// Exchange the credentials now and cache the result (a fixed provider
    /// returns its context).
    ///
    /// # Errors
    /// The `authn_resolver` error.
    pub async fn exchange(&self) -> Result<SecurityContext, AuthNResolverError> {
        let Some((authn, creds)) = &self.exchanger else {
            return self.cached().ok_or_else(|| {
                AuthNResolverError::Internal("no S2S context configured".to_owned())
            });
        };
        let request = ClientCredentialsRequest {
            client_id: creds.client_id.clone(),
            client_secret: SecretString::from(creds.secret().to_owned()),
            scopes: Vec::new(),
        };
        let ctx = authn
            .exchange_client_credentials(&request)
            .await?
            .security_context;
        *self.cached.write().unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
        Ok(ctx)
    }

    /// The cached context, exchanging first when there is none.
    ///
    /// # Errors
    /// The `authn_resolver` error of the exchange.
    pub async fn get(&self) -> Result<SecurityContext, AuthNResolverError> {
        match self.cached() {
            Some(ctx) => Ok(ctx),
            None => self.exchange().await,
        }
    }

    fn cached(&self) -> Option<SecurityContext> {
        self.cached
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Whether an exchange failure is a deterministic misconfiguration (the
/// credentials were rejected) that must fail gear startup; other failures
/// (no plugin yet, service unavailable) are retried.
#[must_use]
pub fn exchange_error_is_fatal(err: &AuthNResolverError) -> bool {
    matches!(
        err,
        AuthNResolverError::Unauthorized(_) | AuthNResolverError::TokenAcquisitionFailed(_)
    )
}

#[cfg(test)]
#[path = "s2s_tests.rs"]
mod tests;
