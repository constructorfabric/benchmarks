//! API-key auth plugin (`...~cf.core.oagw.apikey.v1`).
//!
//! Injects an API key resolved from the credential store into an outbound
//! header or query parameter. Credentials are resolved at request time by
//! `cred://` reference and are never logged or serialized.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::models::plugin_gts::AUTH_APIKEY;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};
use credstore_sdk::{CredStoreClientV1, SecretRef};

/// Configuration keys for the API-key plugin.
pub mod keys {
    /// `key_ref` — `cred://` reference resolving to the API key.
    pub const KEY_REF: &str = "key_ref";
    /// `header_name` — outbound header carrying the key (default `X-API-Key`).
    pub const HEADER_NAME: &str = "header_name";
    /// `query_param` — when set, inject the key as this query parameter instead.
    pub const QUERY_PARAM: &str = "query_param";
}

/// API-key auth plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Create the plugin with a credential-store client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }

    /// Resolve a `cred://{name}` reference against the credential store,
    /// returning the secret bytes. Strips the `cred://` prefix.
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<bytes::Bytes, PluginError> {
        let name = reference.trim_start_matches("cred://");
        let secret_ref = SecretRef::new(name.to_owned()).map_err(|e| PluginError::Config {
            message: format!("invalid key_ref: {e}"),
        })?;
        match self.credstore.get(ctx, &secret_ref).await {
            Ok(Some(resp)) => Ok(bytes::Bytes::copy_from_slice(resp.value.as_bytes())),
            Ok(None) => Err(PluginError::auth(format!(
                "credential {reference} is inaccessible to the current tenant"
            ))),
            Err(e) => Err(PluginError::Internal {
                message: format!("credential store lookup failed for {reference}: {e}"),
            }),
        }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let config = ctx.config;
        let Some(key_ref) = config.get(keys::KEY_REF).and_then(|v| v.as_str()) else {
            return Err(PluginError::Config {
                message: format!("apikey plugin requires `{}`", keys::KEY_REF),
            });
        };
        let key = self.resolve(ctx.security_context, key_ref).await?;
        let key_text = String::from_utf8_lossy(&key).into_owned();

        if let Some(query_param) = config.get(keys::QUERY_PARAM).and_then(|v| v.as_str()) {
            if !query_param.is_empty() {
                ctx.query.push((query_param.to_owned(), key_text));
                return Ok(());
            }
        }
        let header_name = config
            .get(keys::HEADER_NAME)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("X-API-Key");
        ctx.headers.push((header_name.to_owned(), key_text));
        Ok(())
    }
}
