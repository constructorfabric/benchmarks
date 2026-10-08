//! Gear S2S identity: `client_credentials` exchanged through authn-resolver (cached).

use std::sync::Arc;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use secrecy::SecretString;
use tokio::sync::RwLock;
use toolkit_security::SecurityContext;

/// Lazily obtains and caches the gear's S2S security context.
pub struct S2sContextProvider {
    authn: Option<Arc<dyn AuthNResolverClient>>,
    client_id: String,
    client_secret: String,
    cached: RwLock<Option<SecurityContext>>,
}

impl S2sContextProvider {
    #[must_use]
    pub fn new(authn: Arc<dyn AuthNResolverClient>, client_id: String, client_secret: String) -> Self {
        Self {
            authn: Some(authn),
            client_id,
            client_secret,
            cached: RwLock::new(None),
        }
    }

    /// Fixed context (tests).
    #[must_use]
    pub fn fixed(ctx: SecurityContext) -> Self {
        Self {
            authn: None,
            client_id: String::new(),
            client_secret: String::new(),
            cached: RwLock::new(Some(ctx)),
        }
    }

    /// Returns the cached S2S context, exchanging credentials on first use.
    ///
    /// # Errors
    /// Exchange failure text (authn-resolver not ready, bad credentials).
    pub async fn get(&self) -> Result<SecurityContext, String> {
        if let Some(c) = self.cached.read().await.as_ref() {
            return Ok(c.clone());
        }
        let authn = self
            .authn
            .as_ref()
            .ok_or_else(|| "no authn resolver configured".to_owned())?;
        let mut guard = self.cached.write().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let res = authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: self.client_id.clone(),
                client_secret: SecretString::from(self.client_secret.clone()),
                scopes: vec![],
            })
            .await
            .map_err(|e| format!("client credentials exchange failed: {e}"))?;
        *guard = Some(res.security_context.clone());
        Ok(res.security_context)
    }
}
