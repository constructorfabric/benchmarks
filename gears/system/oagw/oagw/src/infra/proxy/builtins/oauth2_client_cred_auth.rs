//! The `oauth2_client_cred` / `oauth2_client_cred_basic` auth plugins
//! (ADR-0008).
//!
//! Configuration:
//!
//! ```json
//! { "token_endpoint": "https://idp/oauth/token",
//!   "client_id_ref": "cred://...", "client_secret_ref": "cred://...",
//!   "scopes": "read write" }
//! ```
//!
//! `token_endpoint` may be replaced by `issuer_url` for OIDC discovery. The
//! bearer token is fetched with [`toolkit_auth::oauth2::fetch_token`] (no
//! background watcher: the gear is multi-tenant, so it manages its own cache)
//! and cached in [`pingora_memory_cache::MemoryCache`] keyed by
//! `(tenant, subject, auth method, config)`; the entry TTL is
//! `min(configured ttl, expires_in − 30 s)`. Failed fetches are not cached.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::header::AUTHORIZATION;
use axum::http::header::HeaderValue;
use pingora_memory_cache::{CacheStatus, MemoryCache};
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString};
use url::Url;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext, Secret, SecretResolver};

const PLUGIN_TYPE: &str = "cf.core.oagw.auth_plugin.v1";
/// Safety margin subtracted from the IdP-reported `expires_in` (ADR-0008).
const EXPIRY_MARGIN: u64 = 30;

/// A cached token that remembers its key so a `u64` hash collision in
/// `TinyUfo` can never hand out another tenant's credential.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Secret,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Parsed plugin configuration of one request.
#[derive(Debug)]
struct OAuth2PluginConfig {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    fn parse(config: &Value) -> Result<Self, PluginError> {
        let token_endpoint = config
            .get("token_endpoint")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let issuer_url = config
            .get("issuer_url")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(PluginError::invalid(
                "CONFIG_INVALID",
                "oauth2 auth plugin requires `token_endpoint` or `issuer_url`",
            ));
        }
        let required = |key: &str| -> Result<String, PluginError> {
            config
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    PluginError::invalid(
                        "CONFIG_INVALID",
                        format!("oauth2 auth plugin requires `{key}`"),
                    )
                })
        };
        let scopes = config
            .get("scopes")
            .and_then(Value::as_str)
            .map(|scopes| {
                scopes
                    .split([' ', ','])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref: required("client_id_ref")?,
            client_secret_ref: required("client_secret_ref")?,
            scopes,
        })
    }
}

/// Auth plugin performing the client-credentials flow with a token cache.
pub struct OAuth2ClientCredAuthPlugin {
    secrets: Arc<dyn SecretResolver>,
    /// Credentials travel as form fields when `false`, as HTTP Basic when
    /// `true` (`oauth2_client_cred_basic`).
    basic: bool,
    http_config: Option<toolkit_http::HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("basic", &self.basic)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin over `secrets` with the given token cache settings.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        basic: bool,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            secrets,
            basic,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Overrides the HTTP client configuration used for the token endpoint.
    #[must_use]
    pub fn with_http_config(mut self, config: toolkit_http::HttpClientConfig) -> Self {
        self.http_config = Some(config);
        self
    }

    fn tag(&self) -> &'static str {
        if self.basic { "basic" } else { "form" }
    }
}

/// Deterministic hash of the plugin configuration, sorted key by key.
fn hash_config(config: &Value) -> String {
    let mut pairs: Vec<String> = Vec::new();
    if let Some(entries) = config.as_object() {
        for (key, value) in entries {
            let rendered = match value {
                Value::String(value) => value.clone(),
                other => other.to_string(),
            };
            pairs.push(format!("{key}={rendered}"));
        }
    }
    pairs.sort();
    let mut hasher = DefaultHasher::new();
    for pair in pairs {
        pair.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Cache key: tenant, subject, auth method and config hash (ADR-0008).
fn build_cache_key(ctx: &RequestContext, tag: &str, config: &Value) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.tenant_id,
        ctx.subject_id,
        tag,
        hash_config(config)
    )
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        if self.basic {
            "cf.core.oagw.oauth2_client_cred_basic.v1"
        } else {
            "cf.core.oagw.oauth2_client_cred.v1"
        }
    }

    fn plugin_type(&self) -> &str {
        PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = OAuth2PluginConfig::parse(&ctx.config)?;
        let key = build_cache_key(ctx, self.tag(), &ctx.config);
        if let (Some(cached), CacheStatus::Hit) = self.cache.get(&key)
            && cached.key == key
        {
            self.inject(ctx, cached.token.expose())?;
            return Ok(());
        }
        let client_id = self.secrets.resolve(&config.client_id_ref).await?;
        let client_secret = self.secrets.resolve(&config.client_secret_ref).await?;
        let endpoint =
            |raw: &Option<String>, field: &'static str| -> Result<Option<Url>, PluginError> {
                raw.as_ref()
                    .map(|url| {
                        Url::parse(url).map_err(|_| {
                            PluginError::invalid(
                                "CONFIG_INVALID",
                                format!("{field} is not a valid URL"),
                            )
                        })
                    })
                    .transpose()
            };
        let client_config = OAuthClientConfig {
            token_endpoint: endpoint(&config.token_endpoint, "token_endpoint")?,
            issuer_url: endpoint(&config.issuer_url, "issuer_url")?,
            client_id: client_id.expose().to_owned(),
            client_secret: SecretString::new(client_secret.expose()),
            scopes: config.scopes,
            auth_method: if self.basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            http_config: Some(
                self.http_config
                    .clone()
                    .unwrap_or_else(toolkit_http::HttpClientConfig::token_endpoint),
            ),
            ..OAuthClientConfig::default()
        };
        let fetched = toolkit_auth::oauth2::fetch_token(client_config)
            .await
            .map_err(|error| {
                PluginError::new(
                    502,
                    "TOKEN_FETCH_FAILED",
                    format!("the token endpoint rejected the client-credentials exchange: {error}"),
                )
            })?;
        let token = Secret::new(fetched.bearer.expose().to_owned());
        let ttl = cache_ttl(self.cache_ttl, fetched.expires_in);
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: token.clone(),
            },
            Some(ttl),
        );
        self.inject(ctx, token.expose())
    }
}

impl OAuth2ClientCredAuthPlugin {
    fn inject(&self, ctx: &mut RequestContext, token: &str) -> Result<(), PluginError> {
        let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            PluginError::invalid("CONFIG_INVALID", "token is not a valid header value")
        })?;
        ctx.headers.insert(AUTHORIZATION, value);
        ctx.set_attribute("oagw.auth.plugin", self.id());
        Ok(())
    }
}

/// `min(configured ttl, expires_in − 30 s)`, never below one second.
fn cache_ttl(configured: Duration, expires_in: Duration) -> Duration {
    let floor = Duration::from_secs(1);
    let margin = Duration::from_secs(EXPIRY_MARGIN);
    expires_in
        .checked_sub(margin)
        .unwrap_or(floor)
        .min(configured)
        .max(floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::LiteralSecretResolver;
    use bytes::Bytes;
    use uuid::Uuid;

    fn plugin(basic: bool) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            Arc::new(LiteralSecretResolver),
            basic,
            Duration::from_secs(300),
            64,
        )
        .with_http_config(toolkit_http::HttpClientConfig::for_testing())
    }

    fn ctx(config: Value) -> RequestContext {
        RequestContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            method: "POST".to_owned(),
            path: "/v1/chat".to_owned(),
            query: String::new(),
            headers: axum::http::HeaderMap::new(),
            body: Bytes::new(),
            config,
            attributes: Default::default(),
        }
    }

    fn config(token_endpoint: String) -> Value {
        serde_json::json!({
            "token_endpoint": token_endpoint,
            "client_id_ref": "client-id",
            "client_secret_ref": "client-secret",
        })
    }

    fn bearer(ctx: &RequestContext) -> Option<String> {
        ctx.headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    #[test]
    fn config_requires_an_endpoint_and_both_references() {
        let error = OAuth2PluginConfig::parse(&serde_json::json!({})).expect_err("no endpoint");
        assert_eq!(error.status, 400);
        let error =
            OAuth2PluginConfig::parse(&serde_json::json!({ "token_endpoint": "http://x/token" }))
                .expect_err("no client id");
        assert_eq!(error.error_code, "CONFIG_INVALID");
    }

    #[test]
    fn the_cache_key_isolates_tenants_subjects_and_configs() {
        let mut ctx = ctx(serde_json::json!({}));
        let base = build_cache_key(&ctx, "form", &serde_json::json!({ "a": 1 }));
        ctx.tenant_id = Uuid::new_v4();
        let other_tenant = build_cache_key(&ctx, "form", &serde_json::json!({ "a": 1 }));
        assert_ne!(base, other_tenant);
        let other_config = build_cache_key(&ctx, "form", &serde_json::json!({ "a": 2 }));
        assert_ne!(base, other_config);
        let other_method = build_cache_key(&ctx, "basic", &serde_json::json!({ "a": 1 }));
        assert_ne!(base, other_method);
    }

    #[test]
    fn config_hashes_are_order_independent() {
        let first = hash_config(&serde_json::json!({ "a": "1", "b": "2" }));
        let second = hash_config(&serde_json::json!({ "b": "2", "a": "1" }));
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn exchanges_credentials_and_caches_the_token() {
        let server = httpmock::MockServer::start();
        let token_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"cached-token","token_type":"Bearer","expires_in":3600}"#);
        });
        let plugin = plugin(false);
        let endpoint = format!("http://localhost:{}/token", server.port());
        let mut ctx = ctx(config(endpoint));

        plugin.authenticate(&mut ctx).await.expect("authenticated");
        assert_eq!(bearer(&ctx).as_deref(), Some("Bearer cached-token"));
        assert_eq!(ctx.attribute("oagw.auth.plugin"), Some(plugin.id()));

        plugin.authenticate(&mut ctx).await.expect("second call");
        assert_eq!(
            token_mock.calls(),
            1,
            "the second call must be served from the cache"
        );
    }

    #[test]
    fn the_ttl_is_capped_and_never_negative() {
        assert_eq!(
            cache_ttl(Duration::from_secs(300), Duration::from_secs(3600)),
            Duration::from_secs(300)
        );
        assert_eq!(
            cache_ttl(Duration::from_secs(60), Duration::from_secs(300)),
            Duration::from_secs(60)
        );
        assert_eq!(
            cache_ttl(Duration::from_secs(300), Duration::from_secs(10)),
            Duration::from_secs(1)
        );
    }
}
