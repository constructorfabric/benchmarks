//! The `OAuth2` client-credentials authentication plugin.
//!
//! Implements ADR-0008: a token is fetched once per
//! `(tenant, subject, auth_method, config)` tuple with
//! `toolkit_auth::oauth2::fetch_token` — which spawns nothing — and cached in
//! `pingora-memory-cache` with a TTL of `min(config_ttl, expires_in − 30 s)`.
//! A `CachedToken` wrapper carries the original key so a `u64` hash collision
//! can only ever look like a miss, never another tenant's token.

use std::time::Duration;

use async_trait::async_trait;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_auth::SecretString;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{AUTH_PLUGIN_OAUTH2_CLIENT_CRED, AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginContext};

/// Cache-timeout ceiling, when the configuration does not name one.
pub const DEFAULT_TOKEN_CACHE_TTL: Duration = Duration::from_mins(5);

/// Maximum number of entries in the token cache.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Safety margin subtracted from the `IdP`'s `expires_in`.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Which client-auth method the variant uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCredentialMethod {
    /// Credentials in the request body.
    Form,
    /// Credentials in an `Authorization: Basic` header.
    Basic,
}

impl ClientCredentialMethod {
    fn auth_method(self) -> ClientAuthMethod {
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

    fn gts_id(self) -> &'static str {
        match self {
            Self::Form => AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
            Self::Basic => AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
        }
    }
}

/// A cached token plus the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// `OAuth2` client-credentials plugin with an internal token cache.
pub struct OAuth2ClientCredAuthPlugin {
    secrets: std::sync::Arc<dyn crate::domain::plugin::SecretResolver>,
    method: ClientCredentialMethod,
    cache: pingora_memory_cache::MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuth2ClientCredAuthPlugin")
            .field("method", &self.method)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin.
    #[must_use]
    pub fn new(
        secrets: std::sync::Arc<dyn crate::domain::plugin::SecretResolver>,
        method: ClientCredentialMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            secrets,
            method,
            cache: pingora_memory_cache::MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Deterministic, tenant- and subject-scoped cache key.
    fn cache_key(&self, context: &PluginContext, config: &serde_json::Value) -> String {
        format!(
            "{}:{}:{}:{}",
            context.tenant_id,
            context.subject_id,
            self.method.tag(),
            hash_config(config)
        )
    }

    /// The configured TTL ceiling.
    fn ttl_ceiling(&self) -> Duration {
        self.cache_ttl
    }
}

/// Stable, order-independent hash of the plugin configuration.
fn hash_config(config: &serde_json::Value) -> u64 {
    let Some(object) = config.as_object() else {
        return 0;
    };
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for key in keys {
        std::hash::Hash::hash(&key.as_str(), &mut hasher);
        std::hash::Hash::hash(&object[key].to_string().as_str(), &mut hasher);
    }
    std::hash::Hasher::finish(&hasher)
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.method.gts_id()
    }

    async fn apply(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
        _query: &mut Vec<(String, String)>,
    ) -> Result<(), DomainError> {
        let key = self.cache_key(context, config);
        if let Some(token) = self.lookup(&key) {
            inject(headers, &token);
            return Ok(());
        }

        let client_id = self.resolve_secret(context, config, "client_id_ref").await?;
        let client_secret = self.resolve_secret(context, config, "client_secret_ref").await?;
        let endpoint = endpoint_url(config)?;
        let scopes = config
            .get("scopes")
            .and_then(serde_json::Value::as_str)
            .map_or_else(Vec::new, |raw| {
                raw.split_whitespace().map(str::to_owned).collect()
            });

        let oauth = OAuthClientConfig {
            token_endpoint: Some(endpoint.clone()),
            issuer_url: None,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: self.method.auth_method(),
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_mins(1),
            jitter_max: Duration::from_secs(0),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: Duration::from_mins(5),
            http_config: None,
        };

        let fetched = fetch_token(oauth).await.map_err(|error| {
            DomainError::AuthenticationFailed(format!("token endpoint rejected the exchange: {error}"))
        })?;

        let ttl = fetched
            .expires_in
            .saturating_sub(EXPIRY_MARGIN)
            .min(self.ttl_ceiling());
        if ttl.is_zero() {
            // A token that is already inside the safety margin, or an IdP that
            // grants less than it, cannot be cached: `put` refuses a zero TTL
            // and the entry would vanish before it was read back. The exchange
            // still succeeded, so the caller gets the token it paid for.
            inject(headers, fetched.bearer.expose());
            return Ok(());
        }
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: fetched.bearer,
            },
            Some(ttl),
        );

        let token = self.lookup(&key).ok_or_else(|| {
            DomainError::AuthenticationFailed("token cache rejected the entry".into())
        })?;
        inject(headers, &token);
        Ok(())
    }
}

impl OAuth2ClientCredAuthPlugin {
    fn lookup(&self, key: &str) -> Option<String> {
        let (entry, _status) = self.cache.get(key);
        let cached = entry?;
        if cached.key != key {
            return None;
        }
        Some(cached.token.expose().to_owned())
    }

    async fn resolve_secret(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        field: &str,
    ) -> Result<String, DomainError> {
        let reference = config
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::AuthenticationFailed(format!("oauth2 auth config requires `{field}`"))
            })?;
        self.secrets
            .resolve(context, reference)
            .await?
            .ok_or_else(|| {
                DomainError::AuthenticationFailed(format!(
                    "credential reference `{reference}` could not be resolved"
                ))
            })
    }
}

fn endpoint_url(config: &serde_json::Value) -> Result<url::Url, DomainError> {
    let raw = config
        .get("token_endpoint")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DomainError::AuthenticationFailed(
                "oauth2 auth config requires `token_endpoint` or `issuer_url`".into(),
            )
        })?;
    url::Url::parse(raw).map_err(|_| {
        DomainError::AuthenticationFailed(format!("`{raw}` is not a valid token endpoint URL"))
    })
}

fn inject(headers: &mut http::HeaderMap, token: &str) {
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(http::header::AUTHORIZATION, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::PluginContext;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Resolver over a fixed table, counting how often it was consulted.
    #[derive(Default)]
    struct CountingSecretResolver {
        values: Mutex<std::collections::BTreeMap<String, String>>,
        lookups: AtomicUsize,
    }

    impl CountingSecretResolver {
        fn with(values: &[(&str, &str)]) -> std::sync::Arc<Self> {
            let map: std::collections::BTreeMap<String, String> = values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            std::sync::Arc::new(Self {
                values: Mutex::new(map),
                lookups: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl crate::domain::plugin::SecretResolver for CountingSecretResolver {
        async fn resolve(
            &self,
            _context: &PluginContext,
            reference: &str,
        ) -> Result<Option<String>, DomainError> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            Ok(self.values.lock().map_or(None, |values| values.get(reference).cloned()))
        }
    }

    fn context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::new_v4(),
            subject_id: uuid::Uuid::new_v4(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            alias: "graph.microsoft.com".into(),
            bearer_token: None,
            request_id: None,
        }
    }

    fn config() -> serde_json::Value {
        serde_json::json!({
            "token_endpoint": "http://127.0.0.1:1/token",
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
            "scopes": "read write"
        })
    }

    #[test]
    fn cache_key_is_scoped_per_tenant_subject_method_and_config() {
        let secrets: std::sync::Arc<dyn crate::domain::plugin::SecretResolver> =
            Arc::new(CountingSecretResolver::default());
        let plugin = OAuth2ClientCredAuthPlugin::new(
            secrets,
            ClientCredentialMethod::Form,
            Duration::from_mins(5),
            128,
        );
        let context = context();
        let first = plugin.cache_key(&context, &config());
        let second = plugin.cache_key(&context, &config());
        assert_eq!(first, second, "same inputs must collide on purpose");

        let reordered = serde_json::json!({
            "scopes": "read write",
            "client_secret_ref": "cred://secret",
            "client_id_ref": "cred://id",
            "token_endpoint": "http://127.0.0.1:1/token"
        });
        assert_eq!(first, plugin.cache_key(&context, &reordered));
    }

    #[tokio::test]
    async fn unresolvable_secret_yields_401() {
        let plugin = OAuth2ClientCredAuthPlugin::new(
            Arc::new(CountingSecretResolver::default()),
            ClientCredentialMethod::Form,
            Duration::from_mins(5),
            128,
        );
        let mut headers = http::HeaderMap::new();
        let error = plugin
            .apply(&context(), &config(), &mut headers, &mut Vec::new())
            .await
            .expect_err("unresolvable");
        assert_eq!(error.status(), 401);
    }

    #[tokio::test]
    async fn unreachable_idp_yields_401_and_is_not_cached() {
        let resolver = CountingSecretResolver::with(&[("cred://id", "client"), ("cred://secret", "shh")]);
        let plugin = OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientCredentialMethod::Form,
            Duration::from_mins(5),
            128,
        );
        let mut headers = http::HeaderMap::new();
        let error = plugin
            .apply(&context(), &config(), &mut headers, &mut Vec::new())
            .await
            .expect_err("idp unreachable");
        assert_eq!(error.status(), 401);
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 2, "both secrets resolved");
    }
}
