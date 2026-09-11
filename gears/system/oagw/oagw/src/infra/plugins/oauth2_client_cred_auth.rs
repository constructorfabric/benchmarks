//! OAuth2 client-credentials auth plugin with an internal token cache
//! (`ADR/0008`).
//!
//! Two registered variants share this implementation and differ only in how
//! the client credentials are transmitted to the token endpoint:
//!
//! | GTS id | Client auth |
//! |---|---|
//! | `…auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | Form body |
//! | `…auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | `Authorization: Basic` |
//!
//! Configuration keys (`ctx.config`): `token_endpoint` *or* `issuer_url`,
//! `client_id_ref`, `client_secret_ref`, optional `scopes`.
//!
//! Access tokens are cached per `(tenant, subject, method, config-hash)` with
//! a TTL of `min(configured, expires_in − 30 s)`; failed fetches are never
//! cached.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{AUTH_OAUTH2, AUTH_OAUTH2_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// Safety margin subtracted from an IdP-reported `expires_in`.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// A cached access token together with the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// Client-credential transmission method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuth {
    /// Credentials in the request body.
    Form,
    /// Credentials in the `Authorization` header.
    Basic,
}

impl ClientAuth {
    fn method(self) -> ClientAuthMethod {
        match self {
            Self::Form => ClientAuthMethod::Form,
            Self::Basic => ClientAuthMethod::Basic,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// Deterministic, order-independent hash of the plugin configuration.
fn config_hash(config: &serde_json::Value) -> u64 {
    let mut flat: BTreeMap<String, String> = BTreeMap::new();
    fn walk(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, value, out);
                }
            }
            serde_json::Value::Null => {
                out.insert(prefix.to_owned(), String::new());
            }
            other => {
                out.insert(prefix.to_owned(), other.to_string());
            }
        }
    }
    walk("", config, &mut flat);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hasher as _;
    for (key, value) in flat {
        hasher.write(key.as_bytes());
        hasher.write_u8(0x1f);
        hasher.write(value.as_bytes());
    }
    hasher.finish()
}

fn cache_key(ctx: &RequestContext, auth: ClientAuth) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.runtime.tenant_id,
        ctx.runtime.subject_id,
        auth.tag(),
        config_hash(&ctx.config)
    )
}

/// Parses the plugin configuration into a token exchange request.
struct OAuth2PluginConfig {
    token_endpoint: Option<url::Url>,
    issuer_url: Option<url::Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    fn parse(config: &serde_json::Value) -> Result<Self, DomainError> {
        let token_endpoint = string_field(config, "token_endpoint")
            .map(|raw| url::Url::parse(&raw))
            .transpose()
            .map_err(|_| DomainError::Validation("invalid token_endpoint url".to_owned()))?;
        let issuer_url = string_field(config, "issuer_url")
            .map(|raw| url::Url::parse(&raw))
            .transpose()
            .map_err(|_| DomainError::Validation("invalid issuer_url url".to_owned()))?;
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(DomainError::Validation(
                "exactly one of token_endpoint or issuer_url is required".to_owned(),
            ));
        }
        let client_id_ref = string_field(config, "client_id_ref")
            .or_else(|| string_field(config, "client_id"))
            .ok_or(DomainError::AuthenticationFailed)?;
        let client_secret_ref = string_field(config, "client_secret_ref")
            .or_else(|| string_field(config, "client_secret"))
            .ok_or(DomainError::AuthenticationFailed)?;
        let scopes = string_field(config, "scopes")
            .map(|raw| raw.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }
}

fn string_field(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The OAuth2 client-credentials plugin for one client-auth method.
pub struct OAuth2ClientCredAuthPlugin {
    id: &'static str,
    auth: ClientAuth,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("id", &self.id)
            .field("auth", &self.auth)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin for one client-auth method.
    #[must_use]
    pub fn new(
        id: &'static str,
        auth: ClientAuth,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            id,
            auth,
            cache: MemoryCache::new(cache_capacity.max(1)),
            cache_ttl,
        }
    }

    /// The client-auth method this instance transmits credentials with.
    #[must_use]
    pub fn client_auth(&self) -> ClientAuth {
        self.auth
    }

    fn build_config(
        &self,
        plugin_config: &OAuth2PluginConfig,
        client_id: &str,
        client_secret: &str,
    ) -> OAuthClientConfig {
        let mut config = OAuthClientConfig {
            auth_method: self.auth.method(),
            ..OAuthClientConfig::default()
        };
        config.token_endpoint = plugin_config.token_endpoint.clone();
        config.issuer_url = plugin_config.issuer_url.clone();
        config.client_id = client_id.to_owned();
        config.client_secret = SecretString::new(client_secret.to_owned());
        config.scopes = plugin_config.scopes.clone();
        config
    }

    async fn resolve(&self, ctx: &RequestContext, reference: &str) -> PluginResult<String> {
        let Some(credstore) = ctx.runtime.credstore.clone() else {
            return Err(DomainError::SecretNotFound);
        };
        let secret_ref =
            credstore_sdk::SecretRef::new(reference.strip_prefix("cred://").unwrap_or(reference))
                .map_err(|_| DomainError::Validation(format!("invalid secret_ref '{reference}'")))?;
        match credstore.get(&ctx.runtime.security, &secret_ref).await {
            Ok(Some(response)) => Ok(String::from_utf8_lossy(response.value.as_bytes())
                .trim()
                .to_owned()),
            Ok(None) => Err(DomainError::SecretNotFound),
            Err(_) => Err(DomainError::SecretNotFound),
        }
    }

    async fn fetch_and_cache(
        &self,
        ctx: &mut RequestContext,
        key: &str,
        plugin_config: &OAuth2PluginConfig,
    ) -> PluginResult<()> {
        let client_id = self.resolve(ctx, &plugin_config.client_id_ref).await?;
        let client_secret = self.resolve(ctx, &plugin_config.client_secret_ref).await?;

        let exchange = self.build_config(plugin_config, &client_id, &client_secret);
        let fetched = tokio::time::timeout(ctx.runtime.timeout, async {
            toolkit_auth::oauth2::fetch_token(exchange).await
        })
        .await
        .map_err(|_| DomainError::RequestTimeout)?
        .map_err(|_| DomainError::AuthenticationFailed)?;

        let effective = token_ttl(fetched.expires_in, self.cache_ttl);
        self.cache.put(
            key,
            CachedToken {
                key: key.to_owned(),
                token: fetched.bearer.clone(),
            },
            Some(effective),
        );
        let token = fetched.bearer.expose().to_owned();
        if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
            ctx.headers.insert(http::header::AUTHORIZATION, value);
        }
        Ok(())
    }
}

/// Cached TTL: `min(expires_in − margin, configured ceiling)`.
fn token_ttl(expires_in: Duration, configured: Duration) -> Duration {
    configured.min(expires_in.saturating_sub(TOKEN_EXPIRY_MARGIN))
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let plugin_config = OAuth2PluginConfig::parse(&ctx.config)?;
        let key = cache_key(ctx, self.auth);
        let (hit, _status) = self.cache.get(&key);
        if is_key_match(hit.as_ref(), &key) {
            let token = hit.expect("verified hit").token.expose().to_owned();
            if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                ctx.headers.insert(http::header::AUTHORIZATION, value);
            }
            return Ok(());
        }
        self.fetch_and_cache(ctx, &key, &plugin_config).await
    }
}

/// `true` when a cache hit carries the exact key that was looked up.
fn is_key_match(hit: Option<&CachedToken>, key: &str) -> bool {
    hit.is_some_and(|entry| entry.key == key)
}

/// Builds the Form-method plugin.
#[must_use]
pub fn form_plugin(cache_ttl: Duration, capacity: usize) -> OAuth2ClientCredAuthPlugin {
    OAuth2ClientCredAuthPlugin::new(AUTH_OAUTH2, ClientAuth::Form, cache_ttl, capacity)
}

/// Builds the Basic-method plugin.
#[must_use]
pub fn basic_plugin(cache_ttl: Duration, capacity: usize) -> OAuth2ClientCredAuthPlugin {
    OAuth2ClientCredAuthPlugin::new(AUTH_OAUTH2_BASIC, ClientAuth::Basic, cache_ttl, capacity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ids_are_the_documented_gts_ids() {
        assert_eq!(form_plugin(Duration::from_secs(1), 8).id(), AUTH_OAUTH2);
        assert_eq!(
            basic_plugin(Duration::from_secs(1), 8).id(),
            AUTH_OAUTH2_BASIC
        );
    }

    #[test]
    fn config_requires_exactly_one_endpoint_and_both_refs() {
        assert!(OAuth2PluginConfig::parse(&json!({})).is_err());
        assert!(
            OAuth2PluginConfig::parse(&json!({
                "token_endpoint": "https://idp/token",
                "issuer_url": "https://idp",
                "client_id_ref": "cred://a",
                "client_secret_ref": "cred://b"
            }))
            .is_err()
        );
        let parsed = OAuth2PluginConfig::parse(&json!({
            "token_endpoint": "https://idp/token",
            "client_id_ref": "cred://a",
            "client_secret_ref": "cred://b",
            "scopes": "a b"
        }))
        .expect("valid");
        assert_eq!(parsed.scopes, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn the_config_hash_is_order_independent() {
        let a = config_hash(&json!({"a": 1, "b": "x"}));
        let b = config_hash(&json!({"b": "x", "a": 1}));
        let c = config_hash(&json!({"a": 2, "b": "x"}));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn the_expiry_margin_is_subtracted() {
        assert_eq!(
            token_ttl(Duration::from_secs(60), Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            token_ttl(Duration::from_secs(10), Duration::from_secs(30)),
            Duration::ZERO
        );
    }
}
