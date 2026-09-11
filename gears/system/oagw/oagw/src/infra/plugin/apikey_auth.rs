//! `apikey` auth plugin (PRD §5.2 "Authentication Injection").
//!
//! Config keys:
//!
//! | key | required | description |
//! |---|---|---|
//! | `key_ref` | yes | `cred://` reference for the API key |
//! | `header` | no | header to inject into (default `x-api-key`) |
//! | `in` / `placement` | no | `header` (default) or `query` |
//! | `query` | no | query parameter name when placed in the query string |

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, PluginConfig, RequestContext};

/// API-key injection plugin.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    security: Option<toolkit_security::SecurityContext>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over a credential store.
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self {
            credstore,
            security: None,
        }
    }

    /// Supplies the security context used to resolve secrets.
    #[must_use]
    pub fn with_security_context(mut self, ctx: toolkit_security::SecurityContext) -> Self {
        self.security = Some(ctx);
        self
    }

    async fn resolve_key(&self, key_ref: &str) -> Result<String, DomainError> {
        let raw = key_ref.strip_prefix("cred://").unwrap_or(key_ref);
        let reference = credstore_sdk::SecretRef::new(raw.to_owned())
            .map_err(|_| DomainError::SecretNotFound)?;
        let ctx = self
            .security
            .clone()
            .unwrap_or_else(toolkit_security::SecurityContext::anonymous);
        match self.credstore.get(&ctx, &reference).await {
            Ok(Some(response)) => String::from_utf8(response.value.as_bytes().to_vec())
                .map_err(|_| DomainError::SecretNotFound),
            _ => Err(DomainError::SecretNotFound),
        }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        gts::AUTH_APIKEY
    }

    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let Some(key_ref) = config
            .string("key_ref")
            .or_else(|| config.string("secret_ref"))
        else {
            return Err(DomainError::Validation(
                "apikey auth plugin requires `key_ref`".to_owned(),
            ));
        };
        let key = self.resolve_key(key_ref).await?;
        let placement = config
            .string("in")
            .or_else(|| config.string("placement"))
            .unwrap_or("header");
        if placement.eq_ignore_ascii_case("query") {
            let name = config
                .string("query")
                .or_else(|| config.string("param"))
                .unwrap_or("api_key")
                .to_owned();
            ctx.query.retain(|(k, _)| k != &name);
            ctx.query.push((name, key));
        } else {
            let name = config
                .string("header")
                .or_else(|| config.string("param"))
                .unwrap_or("x-api-key");
            let name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| DomainError::Validation(format!("invalid header name `{name}`")))?;
            let value = http::HeaderValue::from_str(&key)
                .map_err(|_| DomainError::Validation("invalid api key value".to_owned()))?;
            ctx.headers.insert(name, value);
        }
        ctx.attributes.set("oagw.auth.plugin", self.id());
        Ok(())
    }
}
