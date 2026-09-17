//! `apikey` auth plugin: static secret injected into a header (ADR-0002).

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};
use crate::infra::credentials::SecretResolver;

/// Default header the API key is sent in.
pub const DEFAULT_HEADER: &str = "authorization";
/// Default prefix placed in front of the resolved secret.
pub const DEFAULT_PREFIX: &str = "Bearer";

/// Auth plugin that resolves a secret and injects it as a header value.
pub struct ApiKeyAuthPlugin {
    secrets: SecretResolver,
}

impl ApiKeyAuthPlugin {
    /// Build the plugin on top of a secret resolver.
    #[must_use]
    pub fn new(secrets: SecretResolver) -> Self {
        Self { secrets }
    }
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        crate::ids::AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let reference = ctx
            .config_str("secret_ref")
            .or_else(|| ctx.config_str("api_key_ref"))
            .ok_or_else(|| PluginError::Config("apikey plugin requires `secret_ref`".to_owned()))?;
        let key = self
            .secrets
            .resolve(&ctx.caller.security_context, reference)
            .await
            .map_err(|error| PluginError::Secret(error.to_string()))?;
        let header = ctx
            .config_str("header")
            .unwrap_or(DEFAULT_HEADER)
            .to_owned();
        let prefix = ctx.config_str("prefix").unwrap_or(DEFAULT_PREFIX);
        let value = if prefix.is_empty() {
            key
        } else {
            format!("{prefix} {key}")
        };
        if let Ok(name) = axum::http::HeaderName::try_from(header.as_str())
            && let Ok(value) = axum::http::HeaderValue::from_str(&value)
        {
            ctx.headers.insert(name, value);
            return Ok(());
        }
        Err(PluginError::Config(format!(
            "apikey plugin produced an invalid `{header}` header"
        )))
    }
}

#[cfg(test)]
#[path = "apikey_auth_tests.rs"]
mod apikey_auth_tests;
