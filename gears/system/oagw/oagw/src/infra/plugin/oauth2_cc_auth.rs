//! `OAuth2ClientCredAuthPlugin` (ADR-0008).
//!
//! Resolves `client_id_ref` / `client_secret_ref` from the CredStore, runs
//! RFC 6749 §4.4 via [`toolkit_auth::oauth2::fetch_token`] and caches the
//! resulting bearer token. The cache key is per tenant + subject + client auth
//! method + config hash, and the cached value carries its own key back so a
//! `u64` hash collision can never hand one caller another caller's token.
//!
//! Config keys (ADR-0008): `token_endpoint` *or* `issuer_url`,
//! `client_id_ref`, `client_secret_ref`, `scopes`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, TokenError};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::gts::{AUTH_PLUGIN_OAUTH2_CC_BASIC_INSTANCE, AUTH_PLUGIN_OAUTH2_CC_INSTANCE};
use crate::domain::plugin::{AuthPlugin, PluginContext};
use crate::infra::plugin::SecretResolver;

/// Safety margin subtracted from the IdP-reported `expires_in`.
pub const EXPIRY_MARGIN_SECS: u64 = 30;
/// Default cache ceiling (ADR-0008, `token_cache_ttl_secs`).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default cache capacity (ADR-0008, `token_cache_capacity`).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Smallest lifetime a token is cached for (seconds).
///
/// A credential whose `expires_in` leaves only a fraction of a second after the
/// safety margin is still cached for this floor, so a burst of requests does
/// not hammer the token endpoint while keeping the refresh cadence short.
pub const MIN_TOKEN_CACHE_TTL_SECS: u64 = 1;

/// A cached bearer token, carrying the key it was stored under.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// Client-auth method of the registered variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientAuth {
    /// Credentials in the request body (RFC 6749 §2.3.1 alternative).
    Form,
    /// Credentials in the `Authorization` header (RFC 6749 §2.3.1).
    Basic,
}

impl ClientAuth {
    /// `ClientAuthMethod` of the toolkit token fetcher.
    fn method(self) -> ClientAuthMethod {
        match self {
            Self::Form => ClientAuthMethod::Form,
            Self::Basic => ClientAuthMethod::Basic,
        }
    }

    /// Cache-key and registry-identifier tag.
    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// OAuth2 client-credentials credential injection.
#[derive(Clone)]
pub struct OAuth2ClientCredAuthPlugin {
    secrets: Arc<SecretResolver>,
    auth_method: ClientAuth,
    cache: Arc<MemoryCache<String, CachedToken>>,
    cache_ttl: Duration,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Creates the `Form` variant (`oauth2_client_cred.v1`).
    #[must_use]
    pub fn form(secrets: Arc<SecretResolver>) -> Self {
        Self::new(secrets, ClientAuth::Form)
    }

    /// Creates the `Basic` variant (`oauth2_client_cred_basic.v1`).
    #[must_use]
    pub fn basic(secrets: Arc<SecretResolver>) -> Self {
        Self::new(secrets, ClientAuth::Basic)
    }

    fn new(secrets: Arc<SecretResolver>, auth_method: ClientAuth) -> Self {
        Self {
            secrets,
            auth_method,
            cache: Arc::new(MemoryCache::new(DEFAULT_TOKEN_CACHE_CAPACITY)),
            cache_ttl: Duration::from_secs(DEFAULT_TOKEN_CACHE_TTL_SECS),
        }
    }

    /// Overrides the cache capacity and TTL (used by tests and tuning).
    #[must_use]
    pub fn with_cache(mut self, capacity: usize, ttl: Duration) -> Self {
        self.cache = Arc::new(MemoryCache::new(capacity));
        self.cache_ttl = ttl;
        self
    }

    fn cache_key(&self, security_context: &SecurityContext, config: &serde_json::Value) -> String {
        format!(
            "{}:{}:{}:{:016x}",
            security_context.subject_tenant_id(),
            security_context.subject_id(),
            self.auth_method.tag(),
            config_hash(config),
        )
    }
}

/// FNV-1a over the canonical JSON rendering of the plugin config.
///
/// `serde_json::Value` maps are `BTreeMap`-backed, so `to_string()` is a
/// stable, key-sorted rendering; a 64-bit FNV-1a over it is enough to spread
/// distinct upstream configurations across cache entries.
#[must_use]
pub fn config_hash(config: &serde_json::Value) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in config.to_string().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            ClientAuth::Form => AUTH_PLUGIN_OAUTH2_CC_INSTANCE,
            ClientAuth::Basic => AUTH_PLUGIN_OAUTH2_CC_BASIC_INSTANCE,
        }
    }

    async fn authenticate(
        &self,
        _ctx: &PluginContext,
        security_context: &SecurityContext,
        config: &serde_json::Value,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        let key = self.cache_key(security_context, config);
        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            self.inject(parts, entry.token.expose());
            return Ok(());
        }

        let fetched = self.fetch(security_context, config).await?;
        // A non-positive TTL means "do not cache": in `pingora-memory-cache`
        // a `None` expiry means *never*, so a token that expires within the
        // safety margin must not be stored at all. Anything above that is
        // clamped to a one-second floor so short-lived credentials still
        // refresh instead of being served past their usefulness.
        if let Some(ttl) = cache_ttl_for(self.cache_ttl, fetched.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }
        self.inject(parts, fetched.bearer.expose());
        Ok(())
    }
}

/// Cache lifetime of a fetched token, `None` when it must not be cached.
///
/// The token is cached for the configured ceiling shortened by the IdP-reported
/// `expires_in` minus the safety margin. A result at or below zero is *not*
/// stored: `MemoryCache::put(_, _, None)` would keep it forever.
#[must_use]
pub fn cache_ttl_for(configured: Duration, expires_in: Duration) -> Option<Duration> {
    let remaining = expires_in.saturating_sub(Duration::from_secs(EXPIRY_MARGIN_SECS));
    let ttl = configured.min(remaining);
    (ttl.as_secs() >= 1).then(|| ttl.max(Duration::from_secs(MIN_TOKEN_CACHE_TTL_SECS)))
}

impl OAuth2ClientCredAuthPlugin {
    fn inject(&self, parts: &mut http::request::Parts, token: &str) {
        if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
            parts.headers.insert(http::header::AUTHORIZATION, value);
        }
    }

    async fn fetch(
        &self,
        security_context: &SecurityContext,
        config: &serde_json::Value,
    ) -> Result<toolkit_auth::oauth2::FetchedToken, DomainError> {
        let endpoint = super::config_str(config, "token_endpoint");
        let issuer = super::config_str(config, "issuer_url");
        if endpoint.is_some() == issuer.is_some() {
            return Err(DomainError::Validation(
                "oauth2_client_cred requires exactly one of 'token_endpoint' or 'issuer_url'"
                    .to_owned(),
            ));
        }
        let client_id_ref = super::config_str(config, "client_id_ref").ok_or_else(|| {
            DomainError::Validation("oauth2_client_cred requires 'client_id_ref'".to_owned())
        })?;
        let client_secret_ref =
            super::config_str(config, "client_secret_ref").ok_or_else(|| {
                DomainError::Validation(
                    "oauth2_client_cred requires 'client_secret_ref'".to_owned(),
                )
            })?;

        let client_id = self
            .secrets
            .resolve(security_context, &client_id_ref)
            .await?;
        let client_secret = self
            .secrets
            .resolve(security_context, &client_secret_ref)
            .await?;
        let scopes = config
            .get("scopes")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let oauth_config = OAuthClientConfig {
            token_endpoint: endpoint.and_then(|raw| raw.parse::<url::Url>().ok()),
            issuer_url: issuer.and_then(|raw| raw.parse::<url::Url>().ok()),
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: self.auth_method.method(),
            ..OAuthClientConfig::default()
        };

        toolkit_auth::oauth2::fetch_token(oauth_config)
            .await
            .map_err(|error| {
                tracing::warn!(error = %sanitize_token_error(&error), "oauth2 token fetch failed");
                DomainError::AuthenticationFailed(
                    "the token endpoint rejected the client credentials grant".to_owned(),
                )
            })
    }
}

/// Strips anything that could carry a credential from a token-endpoint error.
fn sanitize_token_error(error: &TokenError) -> String {
    let rendered = error.to_string();
    rendered
        .split_once(':')
        .map_or(rendered.clone(), |(head, _)| head.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn resolver() -> Arc<SecretResolver> {
        Arc::new(SecretResolver::new(Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        ))))
    }

    fn context(tenant: uuid::Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .unwrap()
    }

    #[test]
    fn short_lived_tokens_are_not_cached_forever() {
        let configured = Duration::from_secs(DEFAULT_TOKEN_CACHE_TTL_SECS);
        // A token that outlives the ceiling is cached for the ceiling.
        assert_eq!(
            cache_ttl_for(configured, Duration::from_secs(600)),
            Some(configured)
        );
        // A token that dies sooner is cached no longer than its own lifetime,
        // shortened by the safety margin.
        assert_eq!(
            cache_ttl_for(Duration::from_secs(900), Duration::from_secs(600)),
            Some(Duration::from_secs(570))
        );
        // A token that expires within the margin is not cached at all: `None`
        // would mean "never expire" in `pingora-memory-cache`.
        assert_eq!(cache_ttl_for(configured, Duration::from_secs(30)), None);
        assert_eq!(cache_ttl_for(configured, Duration::from_secs(10)), None);
        // A barely usable token is still cached, for the one-second floor.
        assert_eq!(
            cache_ttl_for(configured, Duration::from_secs(31)),
            Some(Duration::from_secs(1))
        );
        // A short configured ceiling shortens the lifetime further.
        assert_eq!(
            cache_ttl_for(Duration::from_secs(5), Duration::from_secs(600)),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn cache_keys_isolate_tenants_and_configs() {
        let plugin = OAuth2ClientCredAuthPlugin::form(resolver());
        let config = serde_json::json!({"scopes": "read"});
        let other = serde_json::json!({"scopes": "write"});
        let tenant = uuid::Uuid::new_v4();
        let first = context(tenant);
        let second = context(tenant);
        // Distinct callers never share a cache entry, the same caller does.
        assert_ne!(
            plugin.cache_key(&first, &config),
            plugin.cache_key(&context(uuid::Uuid::new_v4()), &config)
        );
        assert_ne!(
            plugin.cache_key(&first, &config),
            plugin.cache_key(&second, &config)
        );
        assert_eq!(
            plugin.cache_key(&first, &config),
            plugin.cache_key(&first, &config)
        );
        assert_ne!(config_hash(&config), config_hash(&other));
        assert_eq!(config_hash(&config), config_hash(&config));
    }

    #[test]
    fn variants_use_distinct_identifiers_and_keys() {
        let secrets = resolver();
        let form = OAuth2ClientCredAuthPlugin::form(secrets.clone());
        let basic = OAuth2ClientCredAuthPlugin::basic(secrets);
        assert_eq!(form.id(), "cf.core.oagw.oauth2_client_cred.v1");
        assert_eq!(basic.id(), "cf.core.oagw.oauth2_client_cred_basic.v1");
        assert_ne!(form.tag_key(), basic.tag_key());
    }

    impl OAuth2ClientCredAuthPlugin {
        fn tag_key(&self) -> &'static str {
            self.auth_method.tag()
        }
    }

    #[test]
    fn errors_are_sanitized() {
        let error = TokenError::Http("GET https://idp/token returned 401: secret=abc".to_owned());
        let sanitized = sanitize_token_error(&error);
        assert!(!sanitized.contains("secret=abc"), "{sanitized}");
        let config = TokenError::ConfigError("token_endpoint is not a valid url".to_owned());
        assert_eq!(sanitize_token_error(&config), "OAuth2 config error");
    }

    #[tokio::test]
    async fn missing_config_is_a_validation_error() {
        let plugin = OAuth2ClientCredAuthPlugin::form(resolver());
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        let error = plugin
            .authenticate(
                &PluginContext {
                    security_context: SecurityContext::anonymous(),
                    upstream_id: uuid::Uuid::nil(),
                    host: "vendor.com".to_owned(),
                    route_id: None,
                    endpoint_host: "api.vendor.com:443".to_owned(),
                    request_id: PluginContext::default_request_id(),
                },
                &SecurityContext::anonymous(),
                &serde_json::json!({}),
                &mut parts,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DomainError::Validation(_)));
    }
}
