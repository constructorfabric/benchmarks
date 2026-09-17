//! Bearer-token auth plugin
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1`).
//!
//! Like `basic.v1`, `bearer.v1` is a *reserved* GTS identifier cataloged in the
//! types-registry and deliberately **not** part of
//! [`AuthPluginRegistry::with_builtins()`] (DESIGN.md "built-in plugins").
//!
//! The implementation is complete: the token is read from the credstore at
//! request time and injected as `Authorization: <prefix> <token>` (prefix
//! default `Bearer`), configurable to any header/scheme pair.

use async_trait::async_trait;
use std::sync::Arc;

pub use crate::domain::gts_helpers::BEARER_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginConfig, PluginError, PluginResult, RequestContext};
use crate::infra::proxy::runtime::PluginRuntime;

/// Default scheme prefix of the injected header value.
pub const DEFAULT_PREFIX: &str = "Bearer";

/// Configuration of the bearer-token auth plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BearerAuthConfig {
    /// Credstore secret reference holding the token.
    pub secret_ref: String,
    /// Scheme prefix of the header value (default `Bearer`).
    pub prefix: String,
    /// Header name (default `authorization`).
    pub header_name: String,
}

impl BearerAuthConfig {
    /// Parse a configuration from a raw `auth.config` value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let s = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        Self {
            secret_ref: s("secret_ref").or_else(|| s("key_ref")).unwrap_or_default(),
            prefix: s("prefix")
                .filter(|p| !p.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PREFIX.to_owned()),
            header_name: s("header_name")
                .map(|h| h.to_ascii_lowercase())
                .unwrap_or_else(|| "authorization".to_owned()),
        }
    }

    /// Parse a configuration from a merged plugin configuration.
    #[must_use]
    pub fn from_plugin_config(config: &PluginConfig) -> Self {
        Self::from_config(&config.config)
    }

    /// True when the configuration names a credential to resolve.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.secret_ref.trim().is_empty()
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// [`PluginError::Internal`] when `secret_ref` is missing.
    pub fn validate(&self) -> PluginResult<()> {
        if self.secret_ref.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: BEARER_AUTH_PLUGIN_ID.to_owned(),
                detail: "auth.config.secret_ref (credstore reference) is required".to_owned(),
            });
        }
        if self.header_name.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: BEARER_AUTH_PLUGIN_ID.to_owned(),
                detail: "auth.config.header_name must not be empty".to_owned(),
            });
        }
        Ok(())
    }

    /// The header value for a resolved token.
    #[must_use]
    pub fn header_value(&self, token: &str) -> String {
        if self.prefix.is_empty() {
            token.to_owned()
        } else {
            format!("{} {token}", self.prefix)
        }
    }
}

/// The bearer-token auth plugin (not registered by `with_builtins()`).
#[derive(Clone)]
pub struct BearerAuthPlugin {
    runtime: Arc<PluginRuntime>,
}

impl std::fmt::Debug for BearerAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerAuthPlugin")
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl BearerAuthPlugin {
    /// A plugin over `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<PluginRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AuthPlugin for BearerAuthPlugin {
    fn id(&self) -> &str {
        BEARER_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        BEARER_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let config = BearerAuthConfig::from_plugin_config(&ctx.config);
        config.validate()?;
        let secret = self
            .runtime
            .resolve_secret(ctx.tenant_id, &ctx.security, &config.secret_ref)
            .await?;
        let token = secret
            .as_str()
            .ok_or_else(|| PluginError::SecretUnavailable {
                plugin_id: BEARER_AUTH_PLUGIN_ID.to_owned(),
                detail: "the referenced bearer token is not valid UTF-8".to_owned(),
            })?;
        ctx.set_header(&config.header_name, &config.header_value(token))?;
        ctx.attributes
            .insert("auth_plugin".to_owned(), BEARER_AUTH_PLUGIN_ID.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::AuthPluginRegistry;
    use crate::infra::proxy::runtime::PluginRuntime;
    use crate::infra::proxy::secrets::InMemorySecretSource;
    use http::HeaderMap;

    fn request_ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: None,
            downstream_headers: HeaderMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id: None,
            route_id: None,
            alias: None,
            trace_id: None,
            config: PluginConfig {
                plugin_id: BEARER_AUTH_PLUGIN_ID.to_owned(),
                position: 0,
                at_upstream_level: true,
                config,
            },
            attributes: Default::default(),
        }
    }

    #[test]
    fn identifier_is_catalog_only() {
        assert!(
            AuthPluginRegistry::with_builtins()
                .get(BEARER_AUTH_PLUGIN_ID)
                .is_none()
        );
        let cfg = BearerAuthConfig::from_config(&serde_json::json!({"secret_ref": "0f9c"}));
        assert_eq!(cfg.prefix, DEFAULT_PREFIX);
        assert_eq!(cfg.header_name, "authorization");
        assert_eq!(cfg.header_value("tok"), "Bearer tok");
    }

    #[tokio::test]
    async fn injects_the_bearer_token() {
        let source = InMemorySecretSource::new();
        source.insert("pat", "ghp_example_token");
        let plugin = BearerAuthPlugin::new(Arc::new(PluginRuntime::new(
            Arc::new(source),
            std::time::Duration::from_secs(300),
            10,
            None,
        )));
        let mut ctx = request_ctx(serde_json::json!({"secret_ref": "pat"}));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get("authorization").unwrap(),
            "Bearer ghp_example_token"
        );
    }

    #[tokio::test]
    async fn a_custom_header_and_prefix_are_honoured() {
        let source = InMemorySecretSource::new();
        source.insert("pat", "tok");
        let plugin = BearerAuthPlugin::new(Arc::new(PluginRuntime::new(
            Arc::new(source),
            std::time::Duration::from_secs(300),
            10,
            None,
        )));
        let mut ctx = request_ctx(serde_json::json!({
            "secret_ref": "pat",
            "header_name": "X-Access-Token",
            "prefix": "Token"
        }));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.headers.get("x-access-token").unwrap(), "Token tok");
    }

    #[test]
    fn an_empty_prefix_injects_the_bare_token() {
        let config = BearerAuthConfig {
            secret_ref: "pat".to_owned(),
            prefix: String::new(),
            header_name: "authorization".to_owned(),
        };
        assert_eq!(config.header_value("tok"), "tok");
        // A blank `prefix` in the wire configuration falls back to the default
        // scheme rather than to the empty string.
        assert_eq!(
            BearerAuthConfig::from_config(&serde_json::json!({"prefix": "  "})).prefix,
            DEFAULT_PREFIX
        );
    }
}
