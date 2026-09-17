//! OAuth2 client-credentials auth plugin
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` and
//! `…~cf.core.oagw.oauth2_client_cred_basic.v1`, ADR 0008).
//!
//! Two variants of the same flow, differing only in how the client
//! authenticates at the token endpoint:
//!
//! * `oauth2_client_cred` — `client_secret_post` (credentials in the form body)
//! * `oauth2_client_cred_basic` — `client_secret_basic` (HTTP Basic header)
//!
//! Tokens are cached in `pingora-memory-cache` under a key that covers the
//! tenant, the subject, the client-auth method and the whole plugin
//! configuration; the effective TTL is
//! `min(cache TTL, expires_in - 30s)` (ADR 0008). The secret is resolved from
//! the credstore at request time and is never logged, cached or echoed.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, PluginConfig, PluginError, PluginResult, RequestContext};
use crate::infra::proxy::runtime::PluginRuntime;

/// Default number of seconds shaved off the issuer's `expires_in`.
pub const EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;

/// How the client authenticates at the token endpoint (ADR 0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Credentials in the `application/x-www-form-urlencoded` body.
    Form,
    /// Credentials as an HTTP Basic authorization header.
    Basic,
}

impl ClientAuthMethod {
    /// Wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }

    /// GTS instance id of the variant.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Self::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        }
    }
}

/// A cached token entry (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedToken {
    /// Raw access token.
    pub access_token: String,
    /// Token type (`Bearer` unless the issuer says otherwise).
    pub token_type: String,
    /// Absolute instant the entry expires.
    pub expires_at: std::time::Instant,
}

impl CachedToken {
    /// True when the entry must be re-fetched.
    #[must_use]
    pub fn is_expired(&self, now: std::time::Instant) -> bool {
        now >= self.expires_at
    }

    /// The `Authorization` header value for this token.
    #[must_use]
    pub fn header_value(&self) -> String {
        let scheme = if self.token_type.is_empty() {
            "Bearer"
        } else {
            &self.token_type
        };
        format!("{scheme} {}", self.access_token)
    }
}

/// Configuration of the OAuth2 client-credentials plugin (`auth.config`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuth2ClientCredAuthConfig {
    /// Token endpoint URL (absolute `https`).
    pub token_url: String,
    /// OAuth2 client id.
    pub client_id: String,
    /// Credstore secret reference (UUID or GTS instance id) holding the client
    /// secret.
    pub client_secret_ref: String,
    /// Requested scopes, in request order.
    pub scopes: Vec<String>,
    /// How the client authenticates at the token endpoint.
    pub client_auth_method: ClientAuthMethod,
    /// Cache TTL override in seconds; `None` uses the gear configuration.
    pub cache_ttl_secs: Option<u64>,
    /// Extra static form parameters (`extra_params`).
    pub extra_params: Vec<(String, String)>,
}

impl OAuth2ClientCredAuthConfig {
    /// Parse a configuration from a raw `auth.config` value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let s = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let client_auth_method = match config.get("client_auth_method").and_then(|v| v.as_str()) {
            Some("client_secret_basic" | "basic") => ClientAuthMethod::Basic,
            _ => ClientAuthMethod::Form,
        };
        let scopes = config
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let extra_params = config
            .get("extra_params")
            .and_then(|v| v.as_object())
            .map(|map| {
                map.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            token_url: s("token_url").unwrap_or_else(|| {
                // `issuer_url` is the OIDC discovery form of the same key.
                s("issuer_url")
                    .map(|issuer| {
                        if issuer.trim_end_matches('/').ends_with("/token") {
                            issuer.trim_end_matches('/').to_owned()
                        } else {
                            format!("{}/token", issuer.trim_end_matches('/'))
                        }
                    })
                    .unwrap_or_default()
            }),
            client_id: s("client_id").unwrap_or_default(),
            client_secret_ref: s("client_secret_ref")
                .or_else(|| s("secret_ref"))
                .unwrap_or_default(),
            scopes,
            client_auth_method,
            cache_ttl_secs: config.get("cache_ttl_secs").and_then(|v| v.as_u64()),
            extra_params,
        }
    }

    /// Parse a configuration from a merged plugin configuration.
    #[must_use]
    pub fn from_plugin_config(config: &PluginConfig) -> Self {
        Self::from_config(&config.config)
    }

    /// True when the configuration is complete enough to attempt a fetch.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.token_url.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret_ref.trim().is_empty()
    }

    /// Effective TTL for a token whose issuer advertised `expires_in`.
    #[must_use]
    pub fn effective_ttl(&self, expires_in: u64, configured_ttl: u64) -> std::time::Duration {
        let capped = expires_in.saturating_sub(EXPIRY_SAFETY_MARGIN_SECS);
        std::time::Duration::from_secs(configured_ttl.min(capped))
    }

    /// The cache TTL this configuration asks for, in seconds.
    #[must_use]
    pub fn ttl_secs(&self, default_ttl_secs: u64) -> u64 {
        self.cache_ttl_secs.unwrap_or(default_ttl_secs).max(1)
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// [`PluginError::Internal`] when a required field is missing or
    /// `token_url` is not an `https` URL (a token endpoint that redirects a
    /// client secret over cleartext is never acceptable).
    pub fn validate(&self) -> PluginResult<()> {
        if self.token_url.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: self.client_auth_method.gts_id().to_owned(),
                detail: "auth.config.token_url is required".to_owned(),
            });
        }
        let parsed = url::Url::parse(&self.token_url).map_err(|e| PluginError::Internal {
            plugin_id: self.client_auth_method.gts_id().to_owned(),
            detail: format!("auth.config.token_url is not a valid URL: {e}"),
        })?;
        if parsed.scheme() != "https" {
            return Err(PluginError::Internal {
                plugin_id: self.client_auth_method.gts_id().to_owned(),
                detail: format!(
                    "auth.config.token_url must use https (got `{}`)",
                    parsed.scheme()
                ),
            });
        }
        if self.client_id.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: self.client_auth_method.gts_id().to_owned(),
                detail: "auth.config.client_id is required".to_owned(),
            });
        }
        if self.client_secret_ref.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: self.client_auth_method.gts_id().to_owned(),
                detail: "auth.config.client_secret_ref (credstore reference) is required"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// The OAuth2 client-credentials auth plugin.
///
/// One implementation serves both registered identifiers; the instance carries
/// its variant so `plugin_type()` reports the right GTS id. The instance owns
/// the shared [`PluginRuntime`] (secret source, token cache, transport).
#[derive(Clone)]
pub struct OAuth2ClientCredAuthPlugin {
    method: ClientAuthMethod,
    runtime: Arc<PluginRuntime>,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("method", &self.method.as_str())
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// The `client_secret_post` variant over `runtime`.
    #[must_use]
    pub fn with_form(runtime: Arc<PluginRuntime>) -> Self {
        Self {
            method: ClientAuthMethod::Form,
            runtime,
        }
    }

    /// The `client_secret_basic` variant over `runtime`.
    #[must_use]
    pub fn with_basic(runtime: Arc<PluginRuntime>) -> Self {
        Self {
            method: ClientAuthMethod::Basic,
            runtime,
        }
    }

    /// Shared helpers (secret source, token cache, transport).
    #[must_use]
    pub const fn runtime(&self) -> &Arc<PluginRuntime> {
        &self.runtime
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.method.gts_id()
    }

    fn plugin_type(&self) -> &str {
        self.method.gts_id()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let config = OAuth2ClientCredAuthConfig::from_plugin_config(&ctx.config);
        if !config.is_configured() {
            return Err(PluginError::Internal {
                plugin_id: self.method.gts_id().to_owned(),
                detail: "auth.config must name token_url, client_id and client_secret_ref"
                    .to_owned(),
            });
        }
        let token = self
            .runtime
            .token_for(&config, ctx.tenant_id, &ctx.security)
            .await?;
        ctx.set_header(http::header::AUTHORIZATION.as_str(), &token.header_value())?;
        ctx.attributes
            .insert("auth_plugin".to_owned(), self.method.gts_id().to_owned());
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
            method: "POST".to_owned(),
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
                plugin_id: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
                position: 0,
                at_upstream_level: true,
                config,
            },
            attributes: Default::default(),
        }
    }

    #[test]
    fn form_is_the_default_variant() {
        let cfg = OAuth2ClientCredAuthConfig::from_config(&serde_json::json!({
            "token_url": "https://auth.example.com/token",
            "client_id": "svc",
            "client_secret_ref": "0f9c"
        }));
        assert_eq!(cfg.client_auth_method, ClientAuthMethod::Form);
        assert!(cfg.is_configured());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn issuer_url_derives_the_token_endpoint() {
        let cfg = OAuth2ClientCredAuthConfig::from_config(&serde_json::json!({
            "issuer_url": "https://auth.example.com/realms/cf",
            "client_id": "svc",
            "client_secret_ref": "cred"
        }));
        assert_eq!(cfg.token_url, "https://auth.example.com/realms/cf/token");
    }

    #[test]
    fn basic_variant_and_scopes() {
        let cfg = OAuth2ClientCredAuthConfig::from_config(&serde_json::json!({
            "token_url": "https://auth.example.com/token",
            "client_id": "svc",
            "client_secret_ref": "0f9c",
            "client_auth_method": "client_secret_basic",
            "scopes": ["read", "write"],
            "cache_ttl_secs": 120,
            "extra_params": {"audience": "api"}
        }));
        assert_eq!(cfg.client_auth_method, ClientAuthMethod::Basic);
        assert_eq!(cfg.scopes, vec!["read".to_owned(), "write".to_owned()]);
        assert_eq!(cfg.cache_ttl_secs, Some(120));
        assert_eq!(
            cfg.extra_params,
            vec![("audience".to_owned(), "api".to_owned())]
        );
        assert_eq!(cfg.ttl_secs(300), 120);
    }

    #[test]
    fn cleartext_token_urls_are_rejected() {
        let cfg = OAuth2ClientCredAuthConfig {
            token_url: "http://auth.example.com/token".to_owned(),
            client_id: "svc".to_owned(),
            client_secret_ref: "0f9c".to_owned(),
            scopes: Vec::new(),
            client_auth_method: ClientAuthMethod::Form,
            cache_ttl_secs: None,
            extra_params: Vec::new(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn ttl_is_capped_by_the_margin() {
        let cfg = OAuth2ClientCredAuthConfig {
            token_url: String::new(),
            client_id: String::new(),
            client_secret_ref: String::new(),
            scopes: Vec::new(),
            client_auth_method: ClientAuthMethod::Form,
            cache_ttl_secs: None,
            extra_params: Vec::new(),
        };
        assert_eq!(
            cfg.effective_ttl(3600, 300),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            cfg.effective_ttl(120, 300),
            std::time::Duration::from_secs(90)
        );
    }

    #[test]
    fn cached_token_expiry() {
        let token = CachedToken {
            access_token: "abc".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_at: std::time::Instant::now(),
        };
        assert!(token.is_expired(std::time::Instant::now()));
        assert_eq!(token.header_value(), "Bearer abc");
    }

    #[tokio::test]
    async fn an_incomplete_binding_is_refused_without_touching_the_network() {
        let runtime = PluginRuntime::null();
        let plugin = OAuth2ClientCredAuthPlugin::with_form(runtime);
        let mut ctx = request_ctx(serde_json::json!({"client_id": "svc"}));
        assert!(plugin.authenticate(&mut ctx).await.is_err());
        assert!(ctx.headers.get(http::header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn an_unreachable_token_endpoint_reports_an_internal_error() {
        let source = InMemorySecretSource::new();
        source.insert("cred", "top-secret");
        let runtime = Arc::new(PluginRuntime::new(
            Arc::new(source),
            Duration::from_secs(300),
            10,
            None,
        ));
        let plugin = OAuth2ClientCredAuthPlugin::with_form(runtime);
        let mut ctx = request_ctx(serde_json::json!({
            "token_url": "https://127.0.0.1:9/token",
            "client_id": "svc",
            "client_secret_ref": "cred"
        }));
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Internal { .. }));
        // The failure text names the transport problem, never the secret.
        assert!(!err.to_string().contains("top-secret"));
    }

    #[tokio::test]
    async fn a_missing_secret_is_reported_as_unavailable() {
        let runtime = Arc::new(PluginRuntime::new(
            Arc::new(InMemorySecretSource::new()),
            Duration::from_secs(300),
            10,
            None,
        ));
        let plugin = OAuth2ClientCredAuthPlugin::with_basic(runtime);
        let mut ctx = request_ctx(serde_json::json!({
            "token_url": "https://auth.example.com/token",
            "client_id": "svc",
            "client_secret_ref": "missing"
        }));
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::SecretUnavailable { .. }));
    }
}
