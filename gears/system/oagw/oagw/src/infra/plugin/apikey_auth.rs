//! API-key auth plugin
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
//!
//! Injects a static API key into the outbound request, sourced from the
//! credstore by reference at request time (PRD "credential injection"): the
//! key material never lives in the upstream configuration, never crosses a
//! log line, and is read only for the request that needs it.
//!
//! The key is presented either as a request header (default `x-api-key`) or as
//! a query-string parameter; an optional scheme prefix (`Bearer`, …) may be
//! prepended to the header value.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginConfig, PluginError, PluginResult, RequestContext};
use crate::infra::proxy::runtime::PluginRuntime;

/// Default header the API key is injected into.
pub const DEFAULT_KEY_HEADER: &str = "x-api-key";

/// How the key is presented to the upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApiKeyLocation {
    /// As a request header (default).
    #[default]
    Header,
    /// As a query-string parameter.
    Query,
}

/// Configuration of the API-key auth plugin (`auth.config`).
///
/// Rejected keys are never echoed: `key_ref` is a credstore secret reference,
/// not the key itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiKeyAuthConfig {
    /// Credential-store secret reference (UUID or GTS instance id) holding the
    /// key material.
    pub key_ref: String,
    /// Header name when `location` is [`ApiKeyLocation::Header`]
    /// (default `x-api-key`).
    pub key_header: String,
    /// Query parameter name when `location` is [`ApiKeyLocation::Query`].
    pub key_query_param: String,
    /// Where the key is presented.
    pub location: ApiKeyLocation,
    /// Optional scheme prefix, e.g. `Bearer` (written without the trailing
    /// space).
    pub prefix: Option<String>,
}

impl ApiKeyAuthConfig {
    /// Parse a configuration from a raw `auth.config` value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let s = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let location = match config.get("location").and_then(|v| v.as_str()) {
            Some("query") => ApiKeyLocation::Query,
            _ => ApiKeyLocation::Header,
        };
        Self {
            key_ref: s("key_ref").or_else(|| s("secret_ref")).unwrap_or_default(),
            key_header: s("key_header")
                .map(|v| v.to_ascii_lowercase())
                .unwrap_or_else(|| DEFAULT_KEY_HEADER.to_owned()),
            key_query_param: s("key_query_param").unwrap_or_else(|| "api_key".to_owned()),
            location,
            prefix: s("prefix").filter(|p| !p.trim().is_empty()),
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
        !self.key_ref.trim().is_empty()
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// [`PluginError::Internal`] when `key_ref` is missing (there is nothing to
    /// inject, and silently forwarding unauthenticated requests would bypass
    /// the upstream's own credentials).
    pub fn validate(&self) -> PluginResult<()> {
        if self.key_ref.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: APIKEY_AUTH_PLUGIN_ID.to_owned(),
                detail: "auth.config.key_ref (credstore secret reference) is required".to_owned(),
            });
        }
        if self.location == ApiKeyLocation::Header && self.key_header.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: APIKEY_AUTH_PLUGIN_ID.to_owned(),
                detail: "auth.config.key_header must not be empty".to_owned(),
            });
        }
        if self.location == ApiKeyLocation::Query && self.key_query_param.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: APIKEY_AUTH_PLUGIN_ID.to_owned(),
                detail: "auth.config.key_query_param must not be empty".to_owned(),
            });
        }
        Ok(())
    }

    /// The header value to inject (`<prefix> <key>` when a prefix is set).
    #[must_use]
    pub fn header_value(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{} {key}", prefix.trim()),
            None => key.to_owned(),
        }
    }
}

/// The API-key auth plugin.
///
/// Construction takes the [`PluginRuntime`] so the plugin can resolve its
/// credential from the credstore at request time.
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    runtime: std::sync::Arc<PluginRuntime>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin")
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl ApiKeyAuthPlugin {
    /// A plugin over `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<PluginRuntime>) -> Self {
        Self { runtime }
    }

    /// The shared runtime.
    #[must_use]
    pub const fn runtime(&self) -> &Arc<PluginRuntime> {
        &self.runtime
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let config = ApiKeyAuthConfig::from_plugin_config(&ctx.config);
        config.validate()?;
        let secret = self
            .runtime
            .resolve_secret(ctx.tenant_id, &ctx.security, &config.key_ref)
            .await?;
        let key = secret
            .as_str()
            .ok_or_else(|| PluginError::SecretUnavailable {
                plugin_id: APIKEY_AUTH_PLUGIN_ID.to_owned(),
                detail: "the referenced API key is not valid UTF-8".to_owned(),
            })?;
        let value = config.header_value(key);
        match config.location {
            ApiKeyLocation::Header => ctx.set_header(&config.key_header, &value)?,
            ApiKeyLocation::Query => {
                // Query presentation keeps the outbound path/query intact and
                // appends the key parameter to `ctx.query`, which the data
                // plane serialises when it builds the upstream URI.
                let mut pairs: Vec<(String, String)> = form_urlencoded::parse(ctx.query.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                pairs.retain(|(name, _)| name.as_str() != config.key_query_param.as_str());
                pairs.push((config.key_query_param.clone(), value));
                ctx.query = form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(pairs)
                    .finish();
            }
        }
        // Bookkeeping for later phases: which credential was used (never its
        // value).
        ctx.attributes
            .insert("auth_plugin".to_owned(), APIKEY_AUTH_PLUGIN_ID.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::runtime::PluginRuntime;
    use crate::infra::proxy::secrets::InMemorySecretSource;
    use http::HeaderMap;
    use std::time::Duration;

    fn request_ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1/x".to_owned(),
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
                plugin_id: APIKEY_AUTH_PLUGIN_ID.to_owned(),
                position: 0,
                at_upstream_level: true,
                config,
            },
            attributes: Default::default(),
        }
    }

    fn runtime_with(key: &str) -> Arc<PluginRuntime> {
        let source = InMemorySecretSource::new();
        source.insert("openai-key", key);
        Arc::new(PluginRuntime::new(
            Arc::new(source),
            Duration::from_secs(300),
            10,
            None,
        ))
    }

    #[tokio::test]
    async fn injects_the_key_header() {
        let plugin = ApiKeyAuthPlugin::new(runtime_with("sk-test-e2e-fake-key"));
        let mut ctx = request_ctx(serde_json::json!({"key_ref": "openai-key"}));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get("x-api-key").unwrap(),
            "sk-test-e2e-fake-key"
        );
        // The key material never leaks into the request attributes.
        assert_eq!(
            ctx.attributes.get("auth_plugin").map(String::as_str),
            Some(APIKEY_AUTH_PLUGIN_ID)
        );
    }

    #[tokio::test]
    async fn honours_the_prefix_and_the_header_override() {
        let plugin = ApiKeyAuthPlugin::new(runtime_with("abc"));
        let mut ctx = request_ctx(serde_json::json!({
            "key_ref": "openai-key",
            "key_header": "X-Custom-Key",
            "prefix": "Bearer"
        }));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.headers.get("x-custom-key").unwrap(), "Bearer abc");
    }

    #[tokio::test]
    async fn injects_into_the_query_when_configured() {
        let plugin = ApiKeyAuthPlugin::new(runtime_with("abc"));
        let mut ctx = request_ctx(serde_json::json!({
            "key_ref": "openai-key",
            "location": "query",
            "key_query_param": "apikey"
        }));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.query, "apikey=abc");
    }

    #[tokio::test]
    async fn a_missing_secret_is_reported_without_the_key() {
        let plugin = ApiKeyAuthPlugin::new(runtime_with("abc"));
        let mut ctx = request_ctx(serde_json::json!({"key_ref": "not-there"}));
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::SecretUnavailable { .. }));
        let rendered = err.to_string();
        assert!(rendered.contains("could not read its secret"), "{rendered}");
        // The failure names the reference, never the material it would have
        // resolved to.
        assert!(rendered.contains("not-there"), "{rendered}");
        assert!(!rendered.contains("sk-"));
    }

    #[tokio::test]
    async fn an_unconfigured_binding_is_refused() {
        let plugin = ApiKeyAuthPlugin::new(runtime_with("abc"));
        let mut ctx = request_ctx(serde_json::json!({}));
        assert!(plugin.authenticate(&mut ctx).await.is_err());
    }

    #[test]
    fn defaults() {
        let cfg = ApiKeyAuthConfig::from_config(&serde_json::json!({"key_ref": "0f9c"}));
        assert_eq!(cfg.key_header, DEFAULT_KEY_HEADER);
        assert_eq!(cfg.location, ApiKeyLocation::Header);
        assert!(cfg.is_configured());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn query_location_and_prefix() {
        let cfg = ApiKeyAuthConfig::from_config(
            &serde_json::json!({"key_ref": "0f9c", "location": "query", "prefix": "Bearer"}),
        );
        assert_eq!(cfg.location, ApiKeyLocation::Query);
        assert_eq!(cfg.prefix.as_deref(), Some("Bearer"));
        assert_eq!(cfg.header_value("k"), "Bearer k");
    }

    #[test]
    fn missing_key_ref_is_rejected() {
        let cfg = ApiKeyAuthConfig::from_config(&serde_json::json!({}));
        assert!(cfg.validate().is_err());
    }
}
