//! The OAuth2 client-credentials auth plugins and their token cache
//! ([ADR-0008](../../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md)).
//!
//! Both registered variants share this implementation: they resolve
//! `client_id_ref` / `client_secret_ref` through the credential store, exchange
//! them for an access token at `token_endpoint` (or at the endpoint discovered
//! from `issuer_url`), and inject `Authorization: Bearer <token>`. The two
//! differ only in how the client authenticates itself to the token endpoint —
//! credentials in the request body ([`OAuth2ClientAuthMethod::Form`]) or in an
//! `Authorization: Basic` header ([`OAuth2ClientAuthMethod::Basic`]).
//!
//! Tokens are cached per [ADR-0008](../../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md):
//! the cache key is `tenant:subject:auth_method:config_hash`, the TTL is
//! `min(token_cache_ttl_secs, expires_in - 30s)`, a token whose TTL does not
//! survive the safety margin is never cached, a failed fetch is never cached,
//! and concurrent refreshes of one key collapse into a single token request.
//! The [`CachedToken`] wrapper re-verifies the key on every hit, so a hash
//! collision can never hand one tenant's token to another.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use http::header::AUTHORIZATION;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_http::HttpClientConfig;
use tracing::warn;
use url::Url;

use crate::domain::error::OagwError;
use crate::domain::model::AuthType;
use crate::infra::plugins::{PluginType, RequestContext, SecretResolver};

use super::{AuthPlugin, config_of, header_value_of, optional_str, required_ref};

// ---------------------------------------------------------------------------
// Configuration keys
// ---------------------------------------------------------------------------

/// `oauth2` — direct token endpoint URL; mutually exclusive with `issuer_url`.
pub const OAUTH2_TOKEN_ENDPOINT: &str = "token_endpoint";
/// `oauth2` — OIDC issuer URL resolved through discovery; mutually exclusive
/// with `token_endpoint`.
pub const OAUTH2_ISSUER_URL: &str = "issuer_url";
/// `oauth2` — `cred://` reference of the OAuth2 client id.
pub const OAUTH2_CLIENT_ID_REF: &str = "client_id_ref";
/// `oauth2` — `cred://` reference of the OAuth2 client secret.
pub const OAUTH2_CLIENT_SECRET_REF: &str = "client_secret_ref";
/// `oauth2` — space-separated OAuth2 scopes requested with the token.
pub const OAUTH2_SCOPES: &str = "scopes";

/// Lifetime margin subtracted from the IdP-reported `expires_in` before a
/// token is cached (ADR-0008).
const TTL_SAFETY_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Default ceiling (seconds) for a cached access token, as documented in
/// [ADR-0008](../../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md)
/// and carried by [`crate::config::OagwConfig`].
const DEFAULT_TTL_SECS: u64 = 300;
/// Default capacity of the token cache, as documented in ADR-0008.
const DEFAULT_CAPACITY: usize = 10_000;

/// How the OAuth2 client authenticates itself to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2ClientAuthMethod {
    /// Credentials in an `application/x-www-form-urlencoded` body.
    Form,
    /// Credentials in an `Authorization: Basic` header.
    Basic,
}

impl OAuth2ClientAuthMethod {
    /// The wire name of the method.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }

    /// The toolkit client-auth method of the token request.
    const fn client_method(self) -> ClientAuthMethod {
        match self {
            Self::Form => ClientAuthMethod::Form,
            Self::Basic => ClientAuthMethod::Basic,
        }
    }
}

/// The token cache knobs of the OAuth2 client-credentials plugins
/// ([ADR-0008](../../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md)).
///
/// Threaded from [`crate::config::OagwConfig`] through
/// [`PluginRegistry::with_builtins`](crate::infra::plugins::PluginRegistry::with_builtins)
/// into the plugin constructors.
#[derive(Debug, Clone)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token's TTL. The effective TTL is
    /// `min(this, expires_in - 30s)`.
    pub ttl: std::time::Duration,
    /// Maximum number of entries; the cache evicts beyond it.
    pub capacity: usize,
    /// HTTP client configuration of the token request; `None` uses the
    /// toolkit's `token_endpoint` preset.
    pub http_config: Option<HttpClientConfig>,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: std::time::Duration::from_secs(DEFAULT_TTL_SECS),
            capacity: DEFAULT_CAPACITY,
            http_config: None,
        }
    }
}

impl TokenCacheConfig {
    /// Builds a configuration with the documented `http_config` default.
    #[must_use]
    pub const fn new(ttl: std::time::Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            http_config: None,
        }
    }

    /// Overrides the HTTP client configuration used for token requests.
    #[must_use]
    pub fn with_http_config(mut self, http_config: HttpClientConfig) -> Self {
        self.http_config = Some(http_config);
        self
    }
}

/// A cached access token, together with the cache key it belongs to.
///
/// `TinyUfo` hashes keys and never compares them, so a collision would
/// otherwise be silent; the key is re-verified on every read.
#[derive(Clone)]
struct CachedToken {
    /// The cache key this token was stored under.
    key: String,
    /// The access token; zeroized on drop and redacted in `Debug`.
    token: SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Where the plugin asks for its token: a configured endpoint, or an issuer
/// resolved through OIDC discovery.
#[derive(Debug, Clone)]
enum TokenSource {
    /// The `token_endpoint` URL of the binding.
    Endpoint(Url),
    /// The `issuer_url` whose `/.well-known/openid-configuration` names the
    /// token endpoint.
    Issuer(Url),
}

impl TokenSource {
    /// Reads the token source from the binding configuration.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when neither or both of `token_endpoint` and
    /// `issuer_url` are configured, or when the configured value is not a URL.
    fn of(config: &Value) -> Result<Self, OagwError> {
        let endpoint = optional_str(config, OAUTH2_TOKEN_ENDPOINT);
        let issuer = optional_str(config, OAUTH2_ISSUER_URL);
        match (endpoint, issuer) {
            (Some(endpoint), None) => Ok(Self::Endpoint(url_of(endpoint, OAUTH2_TOKEN_ENDPOINT)?)),
            (None, Some(issuer)) => Ok(Self::Issuer(url_of(issuer, OAUTH2_ISSUER_URL)?)),
            (Some(_), Some(_)) => Err(OagwError::Validation {
                message: format!(
                    "'{OAUTH2_TOKEN_ENDPOINT}' and '{OAUTH2_ISSUER_URL}' are mutually exclusive"
                ),
            }),
            (None, None) => Err(OagwError::Validation {
                message: format!(
                    "either '{OAUTH2_TOKEN_ENDPOINT}' or '{OAUTH2_ISSUER_URL}' is required"
                ),
            }),
        }
    }
}

/// The requested URL.
///
/// # Errors
/// [`OagwError::Validation`] when the value is not a valid URL.
fn url_of(raw: &str, key: &str) -> Result<Url, OagwError> {
    Url::parse(raw).map_err(|_| OagwError::Validation {
        message: format!("'{key}' is not a valid URL"),
    })
}

/// The TTL of a cached token: `min(config_ttl, expires_in - 30s)` (ADR-0008).
///
/// A token whose lifetime does not survive the safety margin has no cacheable
/// TTL at all.
#[must_use]
fn token_ttl(
    config_ttl: std::time::Duration,
    expires_in: std::time::Duration,
) -> std::time::Duration {
    let with_margin = expires_in.saturating_sub(TTL_SAFETY_MARGIN);
    if with_margin < config_ttl {
        with_margin
    } else {
        config_ttl
    }
}

/// The coarse class of a failed token request, for the log line.
///
/// ADR-0008 keeps the IdP's own response text out of the gear: only the class
/// is logged, never the response body, the client credentials or the token.
fn error_class(error: &toolkit_auth::oauth2::TokenError) -> &'static str {
    match error {
        toolkit_auth::oauth2::TokenError::Http(_) => "http",
        toolkit_auth::oauth2::TokenError::InvalidResponse(_) => "invalid_response",
        toolkit_auth::oauth2::TokenError::UnsupportedTokenType(_) => "unsupported_token_type",
        toolkit_auth::oauth2::TokenError::ConfigError(_) => "config",
        toolkit_auth::oauth2::TokenError::Unavailable(_) => "unavailable",
        toolkit_auth::oauth2::TokenError::InvalidTokenLifetime(_) => "invalid_token_lifetime",
        // The error type is `#[non_exhaustive]`: an unknown variant is reported
        // without detail.
        _ => "other",
    }
}

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` and
/// `...oauth2_client_cred_basic.v1` — OAuth2 client credentials with an
/// internal token cache (ADR-0008).
///
/// Configuration: `token_endpoint` or `issuer_url` (exactly one),
/// `client_id_ref` and `client_secret_ref` (both required), `scopes`
/// (optional).
pub struct OAuth2ClientCredAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    method: OAuth2ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_config: TokenCacheConfig,
    /// One lock per key that is currently being refreshed: the single-flight
    /// guard that turns a burst of misses into one token request.
    refresh_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds one variant of the plugin, resolving client credentials through
    /// `resolver` and caching tokens per `cache_config`.
    #[must_use]
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        method: OAuth2ClientAuthMethod,
        cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            resolver,
            method,
            cache: MemoryCache::new(cache_config.capacity),
            cache_config,
            refresh_locks: DashMap::new(),
        }
    }

    /// The cache key of one request: `tenant:subject:auth_method:config_hash`
    /// (ADR-0008). The tenant and subject isolate concurrent tenants from each
    /// other, the auth method keeps two plugins that share a configuration
    /// apart, and the config hash keeps different scopes or endpoints apart.
    fn cache_key(&self, ctx: &RequestContext, config: &Value) -> String {
        format!(
            "{}:{}:{}:{:x}",
            ctx.security.subject_tenant_id(),
            ctx.security.subject_id(),
            self.method.as_str(),
            config_hash(config)
        )
    }

    /// The cached token of `key`, when it is still valid.
    ///
    /// A `TinyUfo` collision is reported as a miss rather than as another
    /// tenant's token.
    fn cached_token(&self, key: &str) -> Option<SecretString> {
        let (entry, _status) = self.cache.get(key);
        let entry = entry?;
        if entry.key != key {
            return None;
        }
        Some(entry.token)
    }

    /// Stores `token` under `key`, honouring the TTL rule.
    ///
    /// A token whose TTL does not survive the safety margin is not cached: the
    /// next request fetches a fresh one.
    fn store(&self, key: &str, token: &toolkit_auth::oauth2::FetchedToken) {
        let ttl = token_ttl(self.cache_config.ttl, token.expires_in);
        if ttl.is_zero() {
            return;
        }
        self.cache.put(
            key,
            CachedToken {
                key: key.to_owned(),
                token: token.bearer.clone(),
            },
            Some(ttl),
        );
    }

    /// The single-flight lock of `key`, shared by every concurrent miss.
    fn refresh_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.refresh_locks
                .entry(key.to_owned())
                .or_default()
                .value(),
        )
    }

    /// Drops the single-flight lock of `key` once this caller is the last one.
    ///
    /// The map holds one reference and `lock` is this caller's own copy, so a
    /// third reference means somebody is still waiting for the key. Re-creating
    /// the entry on the next miss is cheaper than holding one lock per key
    /// forever.
    fn release_lock(&self, key: &str, lock: &Arc<tokio::sync::Mutex<()>>) {
        if Arc::strong_count(lock) == 2
            && self
                .refresh_locks
                .get(key)
                .is_some_and(|entry| Arc::ptr_eq(entry.value(), lock))
        {
            self.refresh_locks.remove(key);
        }
    }

    /// Resolves the client credentials and exchanges them for an access token.
    ///
    /// A failed exchange is reported, not cached: the next request retries the
    /// IdP.
    async fn fetch_token(
        &self,
        ctx: &RequestContext,
        config: &Value,
        source: &TokenSource,
    ) -> Result<toolkit_auth::oauth2::FetchedToken, OagwError> {
        let client_id_ref = required_ref(config, OAUTH2_CLIENT_ID_REF)?;
        let client_secret_ref = required_ref(config, OAUTH2_CLIENT_SECRET_REF)?;
        let client_id = self.resolver.resolve_cref(ctx, client_id_ref).await?;
        let client_secret = self.resolver.resolve_cref(ctx, client_secret_ref).await?;
        let scopes = optional_str(config, OAUTH2_SCOPES)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut oauth_config = OAuthClientConfig {
            client_id: String::from_utf8_lossy(client_id.as_bytes()).into_owned(),
            client_secret: SecretString::new(String::from_utf8_lossy(client_secret.as_bytes())),
            scopes,
            auth_method: self.method.client_method(),
            http_config: self.cache_config.http_config.clone(),
            ..OAuthClientConfig::default()
        };
        match source {
            TokenSource::Endpoint(endpoint) => oauth_config.token_endpoint = Some(endpoint.clone()),
            TokenSource::Issuer(issuer) => oauth_config.issuer_url = Some(issuer.clone()),
        }

        match fetch_token(oauth_config).await {
            Ok(token) => Ok(token),
            Err(error) => {
                // Only the failure class is logged: the IdP's response text is
                // neither client data nor something an operator can act on, and
                // it may echo request material.
                warn!(
                    auth_method = self.method.as_str(),
                    error_class = error_class(&error),
                    "OAuth2 client credentials token request failed"
                );
                Err(OagwError::DownstreamError {
                    message: "the token endpoint rejected the client credentials grant".to_owned(),
                })
            }
        }
    }

    /// Writes the injected `Authorization` header.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the token is not valid header text.
    fn inject(&self, ctx: &mut RequestContext, token: &SecretString) -> Result<(), OagwError> {
        ctx.request_headers.insert(
            AUTHORIZATION,
            header_value_of(&format!("Bearer {}", token.expose()))?,
        );
        Ok(())
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.method {
            OAuth2ClientAuthMethod::Form => AuthType::OAUTH2_CLIENT_CRED,
            OAuth2ClientAuthMethod::Basic => AuthType::OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = config_of(ctx)?;
        let source = TokenSource::of(&config)?;
        let key = self.cache_key(ctx, &config);

        if let Some(token) = self.cached_token(&key) {
            return self.inject(ctx, &token);
        }
        let lock = self.refresh_lock(&key);
        let _guard = lock.lock().await;
        // Another request of the same key may have refreshed while this one
        // waited for the lock.
        let result = if let Some(token) = self.cached_token(&key) {
            self.inject(ctx, &token)
        } else {
            let token = self.fetch_token(ctx, &config, &source).await?;
            self.store(&key, &token);
            self.inject(ctx, &token.bearer)
        };
        self.release_lock(&key, &lock);
        result
    }
}

/// A sorted, deterministic hash of the binding configuration: the same config
/// always produces the same cache key, and two different configs (different
/// scopes, different references) never share an entry.
fn config_hash(config: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Some(entries) = config.as_object() {
        let mut entries: Vec<(&String, &Value)> = entries.iter().collect();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        for (key, value) in entries {
            key.hash(&mut hasher);
            value.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use std::time::Duration;

    use httpmock::prelude::*;
    use serde_json::json;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::{
        OAUTH2_CLIENT_ID_REF, OAUTH2_CLIENT_SECRET_REF, OAUTH2_ISSUER_URL, OAUTH2_SCOPES,
        OAUTH2_TOKEN_ENDPOINT, OAuth2ClientAuthMethod, OAuth2ClientCredAuthPlugin,
        TokenCacheConfig, config_hash,
    };
    use crate::domain::error::{OagwError, SECRET_NOT_FOUND_GTS_ID};
    use crate::domain::model::AuthType;
    use crate::infra::plugins::{AuthPlugin, PluginType, RequestContext};
    use crate::infra::test_support::secret_store;

    const CLIENT_ID: &str = "test-client";
    const CLIENT_SECRET: &str = "test-client-secret";
    const TOKEN: &str = "idp-issued-access-token";
    const SCOPES: &str = "read write";
    /// The `expires_in` a happy IdP reports.
    const EXPIRES_IN: u64 = 3600;

    fn cache() -> TokenCacheConfig {
        TokenCacheConfig::new(Duration::from_secs(300), 100)
            .with_http_config(toolkit_http::HttpClientConfig::for_testing())
    }

    fn plugin(method: OAuth2ClientAuthMethod) -> OAuth2ClientCredAuthPlugin {
        OAuth2ClientCredAuthPlugin::new(
            secret_store(vec![(CLIENT_ID, CLIENT_ID), (CLIENT_SECRET, CLIENT_SECRET)]),
            method,
            cache(),
        )
    }

    fn form_plugin() -> OAuth2ClientCredAuthPlugin {
        plugin(OAuth2ClientAuthMethod::Form)
    }

    fn token_body(expires_in: u64) -> String {
        format!(r#"{{"access_token":"{TOKEN}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
    }

    fn endpoint_mock(server: &MockServer, expires_in: u64) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body(expires_in));
        })
    }

    /// The token endpoint of `server`.
    fn endpoint(server: &MockServer) -> String {
        format!("http://localhost:{}/token", server.port())
    }

    fn config(token_endpoint: &str) -> serde_json::Value {
        json!({
            OAUTH2_TOKEN_ENDPOINT: token_endpoint,
            OAUTH2_CLIENT_ID_REF: CLIENT_ID,
            OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
            OAUTH2_SCOPES: SCOPES,
        })
    }

    /// One caller, shared by every context of a test: the cache key is derived
    /// from the tenant and the subject, so a test that counts token requests
    /// must keep them identical across its requests.
    fn caller() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("valid security context")
    }

    fn context_for(security: &SecurityContext, config: serde_json::Value) -> RequestContext {
        let mut context = crate::infra::test_support::context_with(config);
        context.security = security.clone();
        context.tenant_id = security.subject_tenant_id();
        context
    }

    #[tokio::test]
    async fn fetches_a_token_and_injects_it_as_a_bearer_value() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, EXPIRES_IN);
        let caller = caller();
        let mut context = context_for(&caller, config(&endpoint(&server)));

        form_plugin()
            .authenticate(&mut context)
            .await
            .expect("authenticated");

        mock.assert_calls(1);
        assert_eq!(
            context
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }

    #[tokio::test]
    async fn sends_the_client_credentials_grant_in_the_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .body_includes("grant_type=client_credentials")
                .body_includes(format!("client_id={CLIENT_ID}"))
                .body_includes(format!("client_secret={CLIENT_SECRET}"))
                .body_includes("scope=read+write");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body(EXPIRES_IN));
        });
        let caller = caller();
        let mut context = context_for(&caller, config(&endpoint(&server)));

        form_plugin()
            .authenticate(&mut context)
            .await
            .expect("authenticated");

        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn the_basic_variant_sends_the_credentials_in_a_basic_header() {
        let server = MockServer::start();
        // base64("test-client:test-client-secret")
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token").header(
                "authorization",
                "Basic dGVzdC1jbGllbnQ6dGVzdC1jbGllbnQtc2VjcmV0",
            );
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body(EXPIRES_IN));
        });
        let caller = caller();
        let mut context = context_for(&caller, config(&endpoint(&server)));
        let basic = plugin(OAuth2ClientAuthMethod::Basic);

        basic
            .authenticate(&mut context)
            .await
            .expect("authenticated");

        mock.assert_calls(1);
        assert_eq!(basic.id(), AuthType::OAUTH2_CLIENT_CRED_BASIC);
        assert_eq!(form_plugin().id(), AuthType::OAUTH2_CLIENT_CRED);
        assert_eq!(basic.plugin_type(), PluginType::Auth);
    }

    #[tokio::test]
    async fn a_second_request_within_the_ttl_reuses_the_cached_token() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, EXPIRES_IN);
        let caller = caller();
        let plugin = form_plugin();

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut first)
            .await
            .expect("first request");
        let mut second = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second request");

        mock.assert_calls(1);
        assert_eq!(
            first
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            second
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
        );
    }

    #[tokio::test]
    async fn a_different_tenant_fetches_its_own_token() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, EXPIRES_IN);
        let plugin = form_plugin();

        let mut first = context_for(&caller(), config(&endpoint(&server)));
        plugin.authenticate(&mut first).await.expect("first tenant");
        let mut second = context_for(&caller(), config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second tenant");

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn a_different_config_fetches_its_own_token() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, EXPIRES_IN);
        let caller = caller();
        let plugin = form_plugin();

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin.authenticate(&mut first).await.expect("first config");
        let mut scoped = config(&endpoint(&server));
        scoped[OAUTH2_SCOPES] = json!("read");
        let mut second = context_for(&caller, scoped);
        plugin
            .authenticate(&mut second)
            .await
            .expect("second config");

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn the_cache_ttl_is_bounded_by_the_configured_ceiling() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, EXPIRES_IN);
        let caller = caller();
        // A one-second ceiling: the IdP offers an hour, so `min()` decides.
        let plugin = OAuth2ClientCredAuthPlugin::new(
            secret_store(vec![(CLIENT_ID, CLIENT_ID), (CLIENT_SECRET, CLIENT_SECRET)]),
            OAuth2ClientAuthMethod::Form,
            TokenCacheConfig::new(Duration::from_secs(1), 100)
                .with_http_config(toolkit_http::HttpClientConfig::for_testing()),
        );

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut first)
            .await
            .expect("first request");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let mut second = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second request");

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn the_cache_ttl_is_bounded_by_the_idp_lifetime() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, 40);
        let caller = caller();
        let plugin = form_plugin();

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut first)
            .await
            .expect("first request");
        let mut second = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second request");

        // `expires_in - 30s` = 10s: the token is cached for that long, well
        // below the configured ceiling.
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn a_token_that_expires_before_the_idp_margin_is_refetched() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, 40);
        let caller = caller();
        // A ceiling above the IdP's remaining lifetime: `expires_in - 30s`
        // decides, so the entry is gone again a moment later.
        let plugin = OAuth2ClientCredAuthPlugin::new(
            secret_store(vec![(CLIENT_ID, CLIENT_ID), (CLIENT_SECRET, CLIENT_SECRET)]),
            OAuth2ClientAuthMethod::Form,
            TokenCacheConfig::new(Duration::from_millis(20), 100)
                .with_http_config(toolkit_http::HttpClientConfig::for_testing()),
        );

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut first)
            .await
            .expect("first request");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut second = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second request");

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn a_token_without_a_cacheable_ttl_is_never_cached() {
        let server = MockServer::start();
        let mock = endpoint_mock(&server, 30);
        let caller = caller();
        let plugin = form_plugin();

        let mut first = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut first)
            .await
            .expect("first request");
        let mut second = context_for(&caller, config(&endpoint(&server)));
        plugin
            .authenticate(&mut second)
            .await
            .expect("second request");

        mock.assert_calls(2);
    }

    #[test]
    fn the_ttl_rule_is_min_of_ceiling_and_expires_in() {
        assert_eq!(
            super::token_ttl(Duration::from_secs(300), Duration::from_secs(3600)),
            Duration::from_secs(300)
        );
        assert_eq!(
            super::token_ttl(Duration::from_secs(300), Duration::from_secs(60)),
            Duration::from_secs(30)
        );
        assert_eq!(
            super::token_ttl(Duration::from_secs(5), Duration::from_secs(3600)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn a_non_positive_ttl_is_a_miss() {
        assert_eq!(
            super::token_ttl(Duration::from_secs(300), Duration::from_secs(30)),
            Duration::ZERO
        );
        assert_eq!(
            super::token_ttl(Duration::from_secs(300), Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn the_cache_key_is_tenant_subject_auth_method_and_config_hash() {
        let plugin = form_plugin();
        let security = caller();
        let context = context_for(&security, config("https://idp.vendor.com/token"));

        let key = plugin.cache_key(&context, &config("https://idp.vendor.com"));

        let parts: Vec<&str> = key.split(':').collect();
        assert_eq!(parts.len(), 4, "{key}");
        assert_eq!(parts[0], security.subject_tenant_id().to_string());
        assert_eq!(parts[1], security.subject_id().to_string());
        assert_eq!(parts[2], "form");
        assert!(!parts[3].is_empty(), "the config hash is part of the key");
    }

    #[test]
    fn the_basic_variant_names_itself_in_the_cache_key() {
        let plugin = plugin(OAuth2ClientAuthMethod::Basic);
        let security = caller();
        let context = context_for(&security, config("https://idp.vendor.com/token"));

        let key = plugin.cache_key(&context, &config("https://idp.vendor.com"));

        assert!(key.contains(":basic:"), "{key}");
    }

    #[test]
    fn the_config_hash_is_deterministic_and_order_independent() {
        let first = json!({ OAUTH2_CLIENT_ID_REF: "a", OAUTH2_CLIENT_SECRET_REF: "b", OAUTH2_SCOPES: "read" });
        let second = json!({ OAUTH2_SCOPES: "read", OAUTH2_CLIENT_SECRET_REF: "b", OAUTH2_CLIENT_ID_REF: "a" });
        let other = json!({ OAUTH2_CLIENT_ID_REF: "a", OAUTH2_CLIENT_SECRET_REF: "b", OAUTH2_SCOPES: "write" });

        assert_eq!(config_hash(&first), config_hash(&first));
        assert_eq!(config_hash(&first), config_hash(&second));
        assert_ne!(config_hash(&first), config_hash(&other));
        assert_ne!(config_hash(&first), config_hash(&json!({})));
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_token_fetch() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body(EXPIRES_IN))
                .delay(Duration::from_millis(150));
        });
        let caller = caller();
        let plugin = std::sync::Arc::new(form_plugin());
        let (mut first, mut second, mut third, mut fourth) = (
            context_for(&caller, config(&endpoint(&server))),
            context_for(&caller, config(&endpoint(&server))),
            context_for(&caller, config(&endpoint(&server))),
            context_for(&caller, config(&endpoint(&server))),
        );

        let (a, b, c, d) = tokio::join!(
            plugin.authenticate(&mut first),
            plugin.authenticate(&mut second),
            plugin.authenticate(&mut third),
            plugin.authenticate(&mut fourth),
        );
        for result in [a, b, c, d] {
            result.expect("authenticated");
        }

        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn a_failed_fetch_is_never_cached() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(500).body("internal server error");
        });
        let caller = caller();
        let plugin = form_plugin();

        let mut first = context_for(&caller, config(&endpoint(&server)));
        let error = plugin.authenticate(&mut first).await.unwrap_err();
        let mut second = context_for(&caller, config(&endpoint(&server)));
        let retried = plugin.authenticate(&mut second).await;

        assert!(
            matches!(error, OagwError::DownstreamError { .. }),
            "{error}"
        );
        assert!(retried.is_err(), "the second request retries the IdP");
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn failures_carry_no_credential_material() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(401).body("bad client credentials");
        });
        let caller = caller();
        let plugin = form_plugin();
        let mut context = context_for(&caller, config(&endpoint(&server)));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        let rendered = format!("{error}");
        assert!(
            !rendered.contains(CLIENT_SECRET)
                && !rendered.contains(CLIENT_ID)
                && !rendered.contains(TOKEN),
            "the failure leaks credential material: {rendered}"
        );
        assert!(
            !format!("{plugin:?}").contains(TOKEN),
            "Debug must not reveal a cached token"
        );
    }

    #[tokio::test]
    async fn a_missing_client_secret_is_a_500_secret_not_found() {
        let server = MockServer::start();
        endpoint_mock(&server, EXPIRES_IN);
        let caller = caller();
        let mut context = context_for(&caller, config(&endpoint(&server)));
        let plugin = OAuth2ClientCredAuthPlugin::new(
            secret_store(vec![(CLIENT_ID, CLIENT_ID)]),
            OAuth2ClientAuthMethod::Form,
            cache(),
        );

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::SecretNotFound), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_NOT_FOUND_GTS_ID);
    }

    #[tokio::test]
    async fn resolves_the_token_endpoint_from_an_issuer_url() {
        let server = MockServer::start();
        let discovery = server.mock(|when, then| {
            when.method(GET).path("/.well-known/openid-configuration");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"token_endpoint":"http://localhost:{}/token"}}"#,
                    server.port()
                ));
        });
        let token = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body(EXPIRES_IN));
        });
        let caller = caller();
        let mut context = context_for(
            &caller,
            json!({
                OAUTH2_ISSUER_URL: format!("http://localhost:{}", server.port()),
                OAUTH2_CLIENT_ID_REF: CLIENT_ID,
                OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
            }),
        );

        form_plugin()
            .authenticate(&mut context)
            .await
            .expect("authenticated");

        discovery.assert_calls(1);
        token.assert_calls(1);
        assert_eq!(
            context
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }

    #[tokio::test]
    async fn a_missing_or_ambiguous_token_source_is_a_400() {
        let plugin = form_plugin();

        let neither = plugin
            .authenticate(&mut crate::infra::test_support::context_with(json!({
                OAUTH2_CLIENT_ID_REF: CLIENT_ID,
                OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
            })))
            .await
            .unwrap_err();
        let both = plugin
            .authenticate(&mut crate::infra::test_support::context_with(json!({
                OAUTH2_TOKEN_ENDPOINT: "https://idp.vendor.com/token",
                OAUTH2_ISSUER_URL: "https://idp.vendor.com",
                OAUTH2_CLIENT_ID_REF: CLIENT_ID,
                OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
            })))
            .await
            .unwrap_err();
        let malformed = plugin
            .authenticate(&mut crate::infra::test_support::context_with(json!({
                OAUTH2_TOKEN_ENDPOINT: "not a url",
                OAUTH2_CLIENT_ID_REF: CLIENT_ID,
                OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
            })))
            .await
            .unwrap_err();

        for error in [neither, both, malformed] {
            assert!(matches!(error, OagwError::Validation { .. }), "{error}");
            assert_eq!(error.http_status(), 400);
        }
    }

    #[tokio::test]
    async fn a_missing_client_ref_is_a_400() {
        let plugin = form_plugin();
        let mut context = crate::infra::test_support::context_with(json!({
            OAUTH2_TOKEN_ENDPOINT: "https://idp.vendor.com/token",
            OAUTH2_CLIENT_SECRET_REF: CLIENT_SECRET,
        }));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        assert_eq!(error.http_status(), 400);
    }

    #[test]
    fn the_plugin_debug_output_carries_no_credential() {
        let plugin = form_plugin();

        assert!(
            !format!("{plugin:?}").contains(CLIENT_SECRET),
            "Debug must not reveal the client secret"
        );
    }
}
