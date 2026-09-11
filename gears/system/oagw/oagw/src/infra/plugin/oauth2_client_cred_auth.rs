//! `cf.core.oagw.oauth2_client_cred.v1` / `…_basic.v1` — OAuth2 Client
//! Credentials (RFC 6749 §4.4) with an internal token cache.
//!
//! Implements `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`:
//! `fetch_token()` (never `Token`, which would spawn a refresh watcher per
//! cache miss), a `pingora-memory-cache` keyed on
//! `(subject_tenant_id, subject_id, auth_method, config_hash)`, a
//! `CachedToken` wrapper that re-verifies the key on hit so a `TinyUfo` hash
//! collision degrades to a miss rather than to another tenant's token, and a
//! `min(config_ttl, expires_in − 30s)` TTL.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderValue, header};
use credstore_sdk::CredStoreClientV1;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use url::Url;

use crate::domain::gts_helpers::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
};
use crate::domain::model::{PluginConfig, config_str};
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};

use super::secret::resolve_secret_ref;

/// Safety margin subtracted from the IdP's `expires_in` before caching.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// A cached bearer token together with the key it was stored under.
///
/// The key is re-checked on every hit: `TinyUfo` hashes keys to `u64` and does
/// not use `Eq` for collision resolution, so without this a collision could
/// hand one tenant another tenant's token.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// Parsed plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OAuth2PluginConfig {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    fn parse(config: &PluginConfig) -> Result<Self, PluginError> {
        let parse_url = |key: &str, raw: String| {
            Url::parse(&raw).map_err(|err| {
                PluginError::InvalidConfig(format!("oauth2 `{key}` is not a valid URL: {err}"))
            })
        };
        let token_endpoint = config_str(config, "token_endpoint")
            .map(|raw| parse_url("token_endpoint", raw))
            .transpose()?;
        let issuer_url = config_str(config, "issuer_url")
            .map(|raw| parse_url("issuer_url", raw))
            .transpose()?;
        match (&token_endpoint, &issuer_url) {
            (None, None) => {
                return Err(PluginError::InvalidConfig(
                    "oauth2 auth plugin requires either `token_endpoint` or `issuer_url`"
                        .to_owned(),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(PluginError::InvalidConfig(
                    "oauth2 auth plugin config keys `token_endpoint` and `issuer_url` are \
                     mutually exclusive"
                        .to_owned(),
                ));
            }
            _ => {}
        }
        let client_id_ref = config_str(config, "client_id_ref").ok_or_else(|| {
            PluginError::InvalidConfig(
                "oauth2 auth plugin requires a `client_id_ref` config key".to_owned(),
            )
        })?;
        let client_secret_ref = config_str(config, "client_secret_ref").ok_or_else(|| {
            PluginError::InvalidConfig(
                "oauth2 auth plugin requires a `client_secret_ref` config key".to_owned(),
            )
        })?;
        let scopes = config_str(config, "scopes")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }
}

/// Short, stable tag for a client-auth method, used in the cache key.
fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// Deterministic hash of the plugin config: sorted key/value pairs, so two
/// upstreams with different scopes never share a cache entry.
fn hash_config(config: &PluginConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut pairs: Vec<(&String, String)> = config
        .iter()
        .map(|(key, value)| (key, value.to_string()))
        .collect();
    pairs.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in pairs {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    hasher.finish()
}

/// Build the cache key for a request.
fn build_cache_key(ctx: &AuthContext, auth_method: ClientAuthMethod) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.security_context.subject_tenant_id(),
        ctx.security_context.subject_id(),
        auth_method_tag(auth_method),
        hash_config(&ctx.config),
    )
}

/// OAuth2 client-credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    http_config: Option<toolkit_http::HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build a plugin variant for one client-auth method.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Override the HTTP client configuration used for the token exchange.
    #[must_use]
    pub fn with_http_config(mut self, http_config: Option<toolkit_http::HttpClientConfig>) -> Self {
        self.http_config = http_config;
        self
    }

    /// Cached token for `key`, if any, with the key verified on hit.
    fn cached(&self, key: &str) -> Option<SecretString> {
        let (entry, _status) = self.cache.get(&key.to_owned());
        let entry = entry?;
        if entry.key == key {
            Some(entry.token)
        } else {
            tracing::warn!(
                target: "oagw.plugin.oauth2",
                "token cache key mismatch on hit; treating as a miss"
            );
            None
        }
    }

    /// Effective cache TTL: `min(config_ttl, expires_in − safety margin)`.
    /// Tokens that expire inside the margin are not cached at all.
    fn effective_ttl(&self, expires_in: Duration) -> Option<Duration> {
        let usable = expires_in.checked_sub(EXPIRY_SAFETY_MARGIN)?;
        if usable.is_zero() {
            return None;
        }
        Some(usable.min(self.cache_ttl))
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
            ClientAuthMethod::Form => "oauth2_client_cred",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form => OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        }
    }

    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let config = OAuth2PluginConfig::parse(&ctx.config)?;
        let key = build_cache_key(ctx, self.auth_method);

        let token = if let Some(token) = self.cached(&key) {
            token
        } else {
            let client_id = resolve_secret_ref(
                self.credstore.as_ref(),
                &ctx.security_context,
                &config.client_id_ref,
            )
            .await?;
            let client_secret = resolve_secret_ref(
                self.credstore.as_ref(),
                &ctx.security_context,
                &config.client_secret_ref,
            )
            .await?;

            let fetched = fetch_token(OAuthClientConfig {
                token_endpoint: config.token_endpoint.clone(),
                issuer_url: config.issuer_url.clone(),
                client_id: client_id.expose().to_owned(),
                client_secret,
                scopes: config.scopes.clone(),
                auth_method: self.auth_method,
                http_config: self.http_config.clone(),
                ..OAuthClientConfig::default()
            })
            .await
            .map_err(|err| {
                // Failed fetches are deliberately not cached: a transient IdP
                // error must self-heal on the next request.
                PluginError::Unauthenticated(format!("oauth2 token exchange failed: {err}"))
            })?;

            if let Some(ttl) = self.effective_ttl(fetched.expires_in) {
                self.cache.put(
                    &key,
                    CachedToken {
                        key: key.clone(),
                        token: fetched.bearer.clone(),
                    },
                    Some(ttl),
                );
            }
            fetched.bearer
        };

        let value = HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|_| {
            PluginError::Internal("acquired access token is not a valid header value".to_owned())
        })?;
        ctx.headers.insert(header::AUTHORIZATION, value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn config(pairs: &[(&str, &str)]) -> PluginConfig {
        let mut config = PluginConfig::new();
        for (key, value) in pairs {
            config.insert((*key).to_owned(), serde_json::json!(value));
        }
        config
    }

    fn ctx(cfg: PluginConfig, tenant: Uuid, subject: Uuid) -> AuthContext {
        AuthContext {
            security_context: SecurityContext::builder()
                .subject_id(subject)
                .subject_tenant_id(tenant)
                .build()
                .expect("context"),
            config: cfg,
            headers: HeaderMap::new(),
            query: vec![],
            upstream_alias: "graph.microsoft.com".to_owned(),
        }
    }

    fn plugin() -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::empty()),
            ClientAuthMethod::Form,
            Duration::from_secs(300),
            16,
        )
    }

    #[test]
    fn config_requires_exactly_one_endpoint_source() {
        assert!(
            OAuth2PluginConfig::parse(&config(&[
                ("client_id_ref", "cred://id"),
                ("client_secret_ref", "cred://secret"),
            ]))
            .is_err(),
            "neither token_endpoint nor issuer_url"
        );
        assert!(
            OAuth2PluginConfig::parse(&config(&[
                ("token_endpoint", "https://idp/token"),
                ("issuer_url", "https://idp"),
                ("client_id_ref", "cred://id"),
                ("client_secret_ref", "cred://secret"),
            ]))
            .is_err(),
            "mutually exclusive"
        );
        let parsed = OAuth2PluginConfig::parse(&config(&[
            ("token_endpoint", "https://idp/token"),
            ("client_id_ref", "cred://id"),
            ("client_secret_ref", "cred://secret"),
            ("scopes", "a b  c"),
        ]))
        .expect("valid");
        assert_eq!(parsed.scopes, vec!["a", "b", "c"]);
    }

    #[test]
    fn config_requires_credential_references() {
        assert!(
            OAuth2PluginConfig::parse(&config(&[("token_endpoint", "https://idp/token")])).is_err()
        );
    }

    #[test]
    fn cache_keys_isolate_tenants_subjects_and_configs() {
        let cfg = config(&[
            ("token_endpoint", "https://idp/token"),
            ("client_id_ref", "cred://id"),
            ("client_secret_ref", "cred://secret"),
        ]);
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let subject = Uuid::new_v4();

        let key_a = build_cache_key(&ctx(cfg.clone(), tenant_a, subject), ClientAuthMethod::Form);
        let key_b = build_cache_key(&ctx(cfg.clone(), tenant_b, subject), ClientAuthMethod::Form);
        assert_ne!(key_a, key_b, "cross-tenant isolation");

        let key_other_subject = build_cache_key(
            &ctx(cfg.clone(), tenant_a, Uuid::new_v4()),
            ClientAuthMethod::Form,
        );
        assert_ne!(key_a, key_other_subject, "cross-subject isolation");

        let key_basic = build_cache_key(
            &ctx(cfg.clone(), tenant_a, subject),
            ClientAuthMethod::Basic,
        );
        assert_ne!(key_a, key_basic, "auth method is part of the key");

        let mut other_scopes = cfg;
        other_scopes.insert("scopes".to_owned(), serde_json::json!("extra"));
        let key_scoped = build_cache_key(
            &ctx(other_scopes, tenant_a, subject),
            ClientAuthMethod::Form,
        );
        assert_ne!(key_a, key_scoped, "config hash is part of the key");
    }

    #[test]
    fn cache_key_is_stable_for_identical_input() {
        let cfg = config(&[
            ("token_endpoint", "https://idp/token"),
            ("client_id_ref", "cred://id"),
            ("client_secret_ref", "cred://secret"),
        ]);
        let tenant = Uuid::new_v4();
        let subject = Uuid::new_v4();
        assert_eq!(
            build_cache_key(&ctx(cfg.clone(), tenant, subject), ClientAuthMethod::Form),
            build_cache_key(&ctx(cfg, tenant, subject), ClientAuthMethod::Form)
        );
    }

    #[test]
    fn ttl_applies_the_safety_margin_and_the_ceiling() {
        let plugin = plugin();
        // 3600s from the IdP, 300s ceiling → the ceiling wins.
        assert_eq!(
            plugin.effective_ttl(Duration::from_secs(3600)),
            Some(Duration::from_secs(300))
        );
        // 120s from the IdP → 120 − 30 = 90s, under the ceiling.
        assert_eq!(
            plugin.effective_ttl(Duration::from_secs(120)),
            Some(Duration::from_secs(90))
        );
        // Inside the safety margin → not cached at all.
        assert_eq!(plugin.effective_ttl(Duration::from_secs(30)), None);
        assert_eq!(plugin.effective_ttl(Duration::from_secs(10)), None);
    }

    #[test]
    fn cache_round_trips_and_rejects_a_key_mismatch() {
        let plugin = plugin();
        let key = "tenant:subject:form:1";
        assert!(plugin.cached(key).is_none());
        plugin.cache.put(
            &key.to_owned(),
            CachedToken {
                key: key.to_owned(),
                token: SecretString::new("tok"),
            },
            Some(Duration::from_secs(60)),
        );
        assert_eq!(
            plugin.cached(key).map(|t| t.expose().to_owned()),
            Some("tok".to_owned())
        );

        // Stored under `key` but carrying a different key → treated as a miss.
        plugin.cache.put(
            &key.to_owned(),
            CachedToken {
                key: "someone-else".to_owned(),
                token: SecretString::new("tok"),
            },
            Some(Duration::from_secs(60)),
        );
        assert!(plugin.cached(key).is_none());
    }

    #[test]
    fn both_variants_declare_their_own_gts_id() {
        let form = plugin();
        assert_eq!(form.plugin_type(), OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID);
        let basic = OAuth2ClientCredAuthPlugin::new(
            Arc::new(MockCredStoreClient::empty()),
            ClientAuthMethod::Basic,
            Duration::from_secs(300),
            16,
        );
        assert_eq!(basic.plugin_type(), OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID);
    }
}
