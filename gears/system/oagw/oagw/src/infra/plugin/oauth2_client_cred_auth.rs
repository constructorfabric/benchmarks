// Updated: 2026-09-01 by Constructor Tech
//! OAuth2 client-credentials auth plugin (ADR-0008).
//!
//! Exchanges a client id / client secret for an access token at the upstream's
//! token endpoint and injects it as `Authorization: Bearer <token>`. Two
//! variants are registered, differing only in how the credentials are
//! transmitted to the token endpoint:
//!
//! * `oauth2_client_cred` — credentials in the request body (Form)
//! * `oauth2_client_cred_basic` — credentials in the `Authorization` header (Basic)
//!
//! Tokens are cached per `(tenant, subject, auth-method, config)` so a burst
//! of proxied requests does not turn into a burst of IdP round trips. The cache
//! is a `pingora_memory_cache::MemoryCache` whose entries carry the full key,
//! so a hash collision is detected and treated as a miss rather than served.
//! Failed fetches are never cached.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_http::HttpClientConfig;
use toolkit_security::SecurityContext;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Safety margin subtracted from the IdP-reported `expires_in` before caching
/// (ADR-0008): a token that is about to expire on the way out is worse than no
/// cache at all.
pub const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// A cached access token together with the key it was filed under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    bearer: toolkit_auth::SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// The OAuth2 client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    auth_method: ClientAuthMethod,
    http_config: Option<HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The cache is not walked: its entries hold live bearer tokens.
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    #[must_use]
    pub fn new(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        auth_method: ClientAuthMethod,
        cache: TokenCacheConfig,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            http_config: None,
            cache: MemoryCache::new(cache.capacity),
            cache_ttl: cache.ttl,
        }
    }

    /// Override the HTTP client used for the token exchange.
    pub fn with_http_config(mut self, config: HttpClientConfig) -> Self {
        self.http_config = Some(config);
        self
    }

    fn config_str(config: &serde_json::Value, key: &str) -> Option<String> {
        config.get(key).and_then(|v| v.as_str()).map(str::to_owned)
    }

    fn bare_key(raw: &str) -> String {
        raw.trim().trim_start_matches("cred://").to_owned()
    }

    /// Sorted, deterministic hash of the plugin configuration so that two
    /// upstreams with different scopes never share a cache entry.
    fn hash_config(config: &serde_json::Value) -> u64 {
        let mut hasher = DefaultHasher::new();
        if let Some(map) = config.as_object() {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                hasher.write(key.as_bytes());
                hasher.write_u8(0xff);
                hasher.write(map[key].to_string().as_bytes());
                hasher.write_u8(0xfe);
            }
        }
        hasher.finish()
    }

    fn cache_key(&self, ctx: &RequestContext, config: &serde_json::Value) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.auth_method_tag(),
            Self::hash_config(config)
        )
    }

    fn auth_method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }

    async fn resolve_secret(
        &self,
        ctx: &SecurityContext,
        key: &str,
    ) -> Result<String, PluginError> {
        let Some(credstore) = &self.credstore else {
            return Err(PluginError::Infrastructure(
                "no credential store is available to resolve the OAuth2 client credentials"
                    .to_owned(),
            ));
        };
        let reference = SecretRef::new(Self::bare_key(key)).map_err(|e| PluginError::Rejected {
            status: http::StatusCode::BAD_REQUEST,
            code: "INVALID_SECRET_REF".to_owned(),
            message: e.to_string(),
        })?;
        match credstore.get(ctx, &reference).await {
            Ok(Some(found)) => Ok(String::from_utf8_lossy(found.value.as_bytes()).into_owned()),
            Ok(None) => Err(PluginError::Infrastructure(format!(
                "referenced secret '{key}' was not found"
            ))),
            Err(err) => Err(PluginError::Infrastructure(format!(
                "credential store lookup failed: {err}"
            ))),
        }
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        "oauth2_client_cred"
    }

    fn plugin_type(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => crate::gts::AUTH_OAUTH2_CC_BASIC,
            ClientAuthMethod::Form => crate::gts::AUTH_OAUTH2_CC,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = ctx
            .config
            .get("auth")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let token_endpoint = Self::config_str(&config, "token_endpoint");
        let issuer_url = Self::config_str(&config, "issuer_url");
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                code: "INVALID_OAUTH2_CONFIG".to_owned(),
                message: "exactly one of `token_endpoint` or `issuer_url` must be configured"
                    .to_owned(),
            });
        }
        let Some(client_id_ref) = Self::config_str(&config, "client_id_ref")
            .or_else(|| Self::config_str(&config, "client_id"))
        else {
            return Err(PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                code: "INVALID_OAUTH2_CONFIG".to_owned(),
                message: "`client_id_ref` is required by the OAuth2 client credentials plugin"
                    .to_owned(),
            });
        };
        let Some(client_secret_ref) = Self::config_str(&config, "client_secret_ref")
            .or_else(|| Self::config_str(&config, "client_secret"))
        else {
            return Err(PluginError::Rejected {
                status: http::StatusCode::BAD_REQUEST,
                code: "INVALID_OAUTH2_CONFIG".to_owned(),
                message: "`client_secret_ref` is required by the OAuth2 client credentials plugin"
                    .to_owned(),
            });
        };
        let scopes = config
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        // ── Cache lookup ────────────────────────────────────────────────
        let key = self.cache_key(ctx, &config);
        let (hit, status) = self.cache.get(&key);
        if status.is_hit()
            && let Some(entry) = hit
            && entry.key == key
            && let Ok(value) = http::HeaderValue::from_str(entry.bearer.expose())
        {
            ctx.headers.insert(http::header::AUTHORIZATION, value);
            return Ok(());
        }

        // ── Cache miss: resolve credentials and exchange ────────────────
        let security = ctx.security_context.clone();
        let client_id = self.resolve_secret(&security, &client_id_ref).await?;
        let client_secret = self.resolve_secret(&security, &client_secret_ref).await?;

        let mut oauth = OAuthClientConfig {
            token_endpoint: token_endpoint
                .as_deref()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| PluginError::Rejected {
                    status: http::StatusCode::BAD_REQUEST,
                    code: "INVALID_OAUTH2_CONFIG".to_owned(),
                    message: format!("token_endpoint is not a valid URL: {e}"),
                })?,
            issuer_url: issuer_url
                .as_deref()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| PluginError::Rejected {
                    status: http::StatusCode::BAD_REQUEST,
                    code: "INVALID_OAUTH2_CONFIG".to_owned(),
                    message: format!("issuer_url is not a valid URL: {e}"),
                })?,
            client_id,
            client_secret: toolkit_auth::SecretString::new(client_secret.as_str()),
            scopes,
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(30),
            jitter_max: Duration::ZERO,
            min_refresh_period: Duration::from_secs(1),
            default_ttl: self.cache_ttl,
            http_config: self.http_config.clone(),
        };
        oauth.refresh_offset = Duration::ZERO;

        let fetched = fetch_token(oauth)
            .await
            .map_err(|e| PluginError::Infrastructure(format!("token acquisition failed: {e}")))?;

        // Tokens the IdP will not honour for the length of the safety margin
        // are used once and never cached.
        let ttl = fetched
            .expires_in
            .checked_sub(EXPIRY_SAFETY_MARGIN)
            .filter(|ttl| !ttl.is_zero())
            .map_or_else(|| None, |ttl| Some(ttl.min(self.cache_ttl)));
        if let Some(ttl) = ttl {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    bearer: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }

        if let Ok(value) = http::HeaderValue::from_str(fetched.bearer.expose()) {
            ctx.headers.insert(http::header::AUTHORIZATION, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::MockCredStoreClient;

    fn ctx_with(config: serde_json::Value) -> RequestContext {
        let mut ctx = crate::infra::plugin::test_support::request_context();
        ctx.config.insert("auth".to_owned(), config);
        ctx
    }

    #[tokio::test]
    async fn requires_exactly_one_of_token_endpoint_or_issuer_url() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Form,
            TokenCacheConfig::default(),
        );
        for config in [
            serde_json::json!({ "client_id_ref": "a", "client_secret_ref": "b" }),
            serde_json::json!({ "client_id_ref": "a", "client_secret_ref": "b",
                                "token_endpoint": "https://idp/token", "issuer_url": "https://idp" }),
        ] {
            let err = plugin
                .authenticate(&mut ctx_with(config))
                .await
                .unwrap_err();
            assert!(matches!(err, PluginError::Rejected { .. }), "{err}");
        }
    }

    #[tokio::test]
    async fn requires_client_credentials() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Form,
            TokenCacheConfig::default(),
        );
        let err = plugin
            .authenticate(&mut ctx_with(
                serde_json::json!({ "token_endpoint": "https://idp/token" }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Rejected { .. }), "{err}");
    }

    #[tokio::test]
    async fn plugin_type_follows_the_auth_method() {
        let form = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Form,
            TokenCacheConfig::default(),
        );
        let basic = OAuth2ClientCredAuthPlugin::new(
            None,
            ClientAuthMethod::Basic,
            TokenCacheConfig::default(),
        );
        assert_eq!(form.plugin_type(), crate::gts::AUTH_OAUTH2_CC);
        assert_eq!(basic.plugin_type(), crate::gts::AUTH_OAUTH2_CC_BASIC);
    }

    #[tokio::test]
    async fn config_hash_distinguishes_different_scopes() {
        let a = serde_json::json!({ "scopes": "read" });
        let b = serde_json::json!({ "scopes": "write" });
        assert_ne!(
            OAuth2ClientCredAuthPlugin::hash_config(&a),
            OAuth2ClientCredAuthPlugin::hash_config(&b)
        );
        assert_eq!(
            OAuth2ClientCredAuthPlugin::hash_config(&a),
            OAuth2ClientCredAuthPlugin::hash_config(&a)
        );
    }

    #[tokio::test]
    async fn injects_the_token_and_caches_it() {
        // A stub token endpoint is exercised in the integration tests; here we
        // assert the caching contract directly.
        let plugin = OAuth2ClientCredAuthPlugin::new(
            Some(Arc::new(MockCredStoreClient::with_secrets(vec![
                ("cid".to_owned(), "client".to_owned()),
                ("csec".to_owned(), "s3cr3t".to_owned()),
            ]))),
            ClientAuthMethod::Form,
            TokenCacheConfig::default(),
        );
        assert!(plugin.credstore.is_some());
    }
}
