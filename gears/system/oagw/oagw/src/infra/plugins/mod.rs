//! Built-in plugin implementations and the data-plane plugin registry.
//!
//! Implements the plugin system specified by ADR 0002 / 0008 / 0009:
//!
//! - **Auth**: `noop`, `apikey`, `oauth2_client_cred` (Form),
//!   `oauth2_client_cred_basic` (Basic).
//! - **Guard**: `required_headers` (request → 400, response → 502).
//! - **Transform**: `request_id` (X-Request-ID propagation).
//!
//! The registry resolves GTS plugin identifiers and custom plugin UUIDs into
//! [`BoundPlugin`] instances; catalog-only identifiers
//! (`basic`/`bearer`/`timeout`/`cors`/`logging`/`metrics`) refuse to bind.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString};
use toolkit_http::HttpClientConfig;
use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{
    AuthError, AuthPlugin, BoundPlugin, ErrorContext, GuardError, GuardPlugin,
    PluginBindingRef, PluginRegistry, RequestContext, ResponseContext, TransformError,
    TransformPlugin,
};
use crate::domain::repo::PluginRepo;
use crate::gts;

/// Registry that resolves plugin references into live instances.
pub struct PluginRegistryImpl {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    token_cache: Arc<MemoryCache<String, CachedToken>>,
    cache_cfg: TokenCacheConfig,
    http_config: HttpClientConfig,
    custom: Arc<dyn PluginRepo>,
}

impl std::fmt::Debug for PluginRegistryImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistryImpl").finish_non_exhaustive()
    }
}

impl PluginRegistryImpl {
    /// Construct the registry with the shared credential client, token cache,
    /// and custom-plugin repository.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache: Arc<MemoryCache<String, CachedToken>>,
        cache_cfg: TokenCacheConfig,
        proxy_timeout: Duration,
        custom: Arc<dyn PluginRepo>,
    ) -> Self {
        let mut http_config = HttpClientConfig::proxy();
        http_config.request_timeout = proxy_timeout;
        Self {
            credstore,
            token_cache,
            cache_cfg,
            http_config,
            custom,
        }
    }

    /// Resolve the upstream auth plugin by its `auth_plugin` GTS type.
    pub fn auth_plugin(&self, plugin_type: &str) -> Result<Box<dyn AuthPlugin>, String> {
        match plugin_type {
            gts::AUTH_NOOP => Ok(Box::new(NoopAuthPlugin) as Box<dyn AuthPlugin>),
            gts::AUTH_APIKEY => Ok(Box::new(ApiKeyAuthPlugin::new(self.credstore.clone()))
                as Box<dyn AuthPlugin>),
            gts::AUTH_OAUTH2_CLIENT_CRED => Ok(self.oauth2(ClientAuthMethod::Form)),
            gts::AUTH_OAUTH2_CLIENT_CRED_BASIC => Ok(self.oauth2(ClientAuthMethod::Basic)),
            _ => Err(format!("unknown auth plugin type '{plugin_type}'")),
        }
    }

    fn oauth2(&self, method: ClientAuthMethod) -> Box<dyn AuthPlugin> {
        Box::new(OAuth2ClientCredAuthPlugin::new(
            method,
            self.credstore.clone(),
            self.token_cache.clone(),
            self.cache_cfg.clone(),
            self.http_config.clone(),
        ))
    }
}

#[async_trait]
impl PluginRegistry for PluginRegistryImpl {
    async fn resolve(&self, binding: &PluginBindingRef) -> Result<BoundPlugin, String> {
        let config = binding.config.clone();
        match binding.plugin_ref.as_str() {
            gts::AUTH_NOOP | gts::AUTH_APIKEY => {
                let auth = self.auth_plugin(&binding.plugin_ref)?;
                Ok(BoundPlugin::Auth(auth, config))
            }
            gts::AUTH_OAUTH2_CLIENT_CRED => {
                Ok(BoundPlugin::Auth(self.oauth2(ClientAuthMethod::Form), config))
            }
            gts::AUTH_OAUTH2_CLIENT_CRED_BASIC => {
                Ok(BoundPlugin::Auth(self.oauth2(ClientAuthMethod::Basic), config))
            }
            gts::GUARD_REQUIRED_HEADERS => Ok(BoundPlugin::Guard(
                Box::new(RequiredHeadersGuardPlugin),
                config,
            )),
            gts::TRANSFORM_REQUEST_ID => Ok(BoundPlugin::Transform(
                Box::new(RequestIdTransformPlugin),
                config,
            )),
            // Catalog-only identifiers must not be bound through the chain.
            gts::AUTH_BASIC
            | gts::AUTH_BEARER
            | gts::GUARD_TIMEOUT
            | gts::GUARD_CORS
            | gts::TRANSFORM_LOGGING
            | gts::TRANSFORM_METRICS => Err(format!(
                "plugin '{}' is catalog-only and cannot be bound",
                binding.plugin_ref
            )),
            other => {
                // Custom plugin by UUID: exists in tenant scope but has no
                // executable backing (Starlark execution is out of scope).
                if let Ok(id) = Uuid::parse_str(other) {
                    if self.custom.get(binding.tenant_id, id).is_some() {
                        Err(format!(
                            "custom plugin '{other}' has no executable backing implementation"
                        ))
                    } else {
                        Err(format!("unknown custom plugin '{other}'"))
                    }
                } else {
                    Err(format!("unknown plugin identifier '{other}'"))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// noop
// ---------------------------------------------------------------------------

/// No-op authentication: forwards the inbound `Authorization` header as-is.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        gts::AUTH_NOOP
    }

    fn plugin_type(&self) -> &'static str {
        gts::AUTH_NOOP
    }

    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), AuthError> {
        // Preserve the caller-provided credentials verbatim.
        if let Some(v) = ctx
            .inbound_header(http::header::AUTHORIZATION.as_str())
            .map(str::to_owned)
        {
            ctx.set_outbound_header(http::header::AUTHORIZATION.as_str(), &v);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// apikey
// ---------------------------------------------------------------------------

/// API-key authentication. Configuration:
///
/// - `header` (default `x-api-key`): the request header carrying the key.
/// - `key_ref` (`cred://`): credential store reference for the *expected*
///   key. `secret_ref` is accepted as an alias.
///
/// The upstream is called with the (possibly normalized) key header intact;
/// when no credential is configured, the key is passed through.
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        gts::AUTH_APIKEY
    }

    fn plugin_type(&self) -> &'static str {
        gts::AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), AuthError> {
        let config = ctx.config;
        let header = config
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("x-api-key");

        // Pass the caller's key through to the upstream.
        if let Some(v) = ctx.inbound_header(header).map(str::to_owned) {
            ctx.set_outbound_header(header, &v);
        }

        // When an expected key is configured, enforce equality.
        let key_ref = config
            .get("key_ref")
            .or_else(|| config.get("secret_ref"))
            .and_then(Value::as_str);
        if let Some(key_ref) = key_ref {
            let expected = resolve_secret_raw(&*self.credstore, ctx, key_ref).await?;
            let provided = ctx.inbound_header(header).unwrap_or("");
            if provided != expected {
                return Err(AuthError::Rejected("invalid API key".to_owned()));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// oauth2 client credentials
// ---------------------------------------------------------------------------

/// Cached OAuth2 bearer token with the cache-key it was stored under.
#[derive(Clone)]
pub struct CachedToken {
    /// The exact cache key this token was stored under (read-time
    /// verification against config drift).
    pub key: String,
    /// The bearer token value.
    pub bearer: String,
}

/// A token-bucket entry for the auth token cache.
pub struct OAuth2ClientCredAuthPlugin {
    auth_method: ClientAuthMethod,
    plugin_id: &'static str,
    plugin_type: &'static str,
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    cache: Arc<MemoryCache<String, CachedToken>>,
    ttl: Duration,
    http_config: HttpClientConfig,
}

impl OAuth2ClientCredAuthPlugin {
    /// Construct the plugin for the given client-auth method.
    #[must_use]
    pub fn new(
        auth_method: ClientAuthMethod,
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        cache: Arc<MemoryCache<String, CachedToken>>,
        cache_cfg: TokenCacheConfig,
        http_config: HttpClientConfig,
    ) -> Self {
        let (plugin_id, plugin_type) = match auth_method {
            ClientAuthMethod::Form => (gts::AUTH_OAUTH2_CLIENT_CRED, gts::AUTH_OAUTH2_CLIENT_CRED),
            ClientAuthMethod::Basic => {
                (gts::AUTH_OAUTH2_CLIENT_CRED_BASIC, gts::AUTH_OAUTH2_CLIENT_CRED_BASIC)
            }
        };
        let ttl = Duration::from_secs(cache_cfg.cache_ttl_secs.max(1));
        Self {
            auth_method,
            plugin_id,
            plugin_type,
            credstore,
            cache,
            ttl,
            http_config,
        }
    }

    /// Stable cache key per ADR 0008:
    /// `{tenant}:{subject}:{auth_method}:{sorted_config_hash}`.
    fn cache_key(&self, ctx: &RequestContext<'_>) -> String {
        let tenant = ctx.tenant_id;
        let subject = ctx.security.subject_id();
        let auth_method = match self.auth_method {
            ClientAuthMethod::Form => "form",
            ClientAuthMethod::Basic => "basic",
        };
        let hash = sorted_config_hash(ctx.config);
        format!("{tenant}:{subject}:{auth_method}:{hash}")
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.plugin_id
    }

    fn plugin_type(&self) -> &'static str {
        self.plugin_type
    }

    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), AuthError> {
        let key = self.cache_key(ctx);

        // Cache hit (with key verification): reuse the token.
        {
            let (cached, status) = self.cache.get(&key);
            if let Some(cached) = cached {
                if cached.key == key && status == pingora_memory_cache::CacheStatus::Hit {
                    ctx.set_outbound_header(http::header::AUTHORIZATION.as_str(), {
                        // RFC 6750
                        format!("Bearer {}", cached.bearer)
                    });
                    return Ok(());
                }
            }
        }

        // Resolve configuration.
        let (token_endpoint, issuer_url) = match (
            ctx.config.get("token_endpoint").and_then(Value::as_str),
            ctx.config.get("issuer_url").and_then(Value::as_str),
        ) {
            (Some(t), None) => (Some(t.to_owned()), None),
            (None, Some(i)) => (None, Some(i.to_owned())),
            (None, None) => {
                return Err(AuthError::Rejected(
                    "oauth2 plugin requires token_endpoint or issuer_url".to_owned(),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(AuthError::Rejected(
                    "token_endpoint and issuer_url are mutually exclusive".to_owned(),
                ));
            }
        };

        let client_id_ref = ctx
            .config
            .get("client_id_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| AuthError::Rejected("client_id_ref is required".to_owned()))?;
        let client_secret_ref = ctx
            .config
            .get("client_secret_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| AuthError::Rejected("client_secret_ref is required".to_owned()))?;
        let scopes: Vec<String> = ctx
            .config
            .get("scopes")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        let client_id = resolve_secret_raw(&*self.credstore, ctx, client_id_ref).await?;
        let client_secret = resolve_secret_raw(&*self.credstore, ctx, client_secret_ref).await?;

        let mut oauth = OAuthClientConfig {
            token_endpoint: token_endpoint
                .and_then(|t| url::Url::parse(&t).ok()),
            issuer_url: issuer_url.and_then(|i| url::Url::parse(&i).ok()),
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            http_config: Some(self.http_config.clone()),
            ..Default::default()
        };
        oauth.min_refresh_period = Duration::from_secs(1);
        oauth.jitter_max = Duration::ZERO;
        oauth.refresh_offset = Duration::from_secs(30);
        oauth.default_ttl = self.ttl;

        let token = match toolkit_auth::oauth2::fetch_token(oauth).await {
            Ok(t) => t,
            Err(e) => {
                return Err(AuthError::Backend(format!(
                    "token fetch failed: {e}"
                )));
            }
        };

        // Effective TTL = min(config ttl, expires_in − 30s safety margin).
        let mut effective = self.ttl;
        let expires = token.expires_in;
        if let Some(safety) = expires.checked_sub(Duration::from_secs(30)) {
            effective = effective.min(safety).max(Duration::from_secs(1));
        }

        let bearer = token.bearer.expose().to_owned();
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                bearer: bearer.clone(),
            },
            Some(effective),
        );

        ctx.set_outbound_header(http::header::AUTHORIZATION.as_str(), {
            format!("Bearer {bearer}")
        });
        Ok(())
    }
}

/// Deterministic hash over the sorted (`key`, `value`) pairs of a config
/// object (ADR 0008 config-key component).
fn sorted_config_hash(config: &Value) -> String {
    use std::collections::BTreeMap;
    let mut pairs = BTreeMap::new();
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            pairs.insert(k.clone(), v.to_string());
        }
    }
    let mut fnv: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in &pairs {
        for b in format!("{k}={v};").bytes() {
            fnv ^= u64::from(b);
            fnv = fnv.wrapping_mul(0x100_0000_01b3);
        }
    }
    format!("{fnv:016x}")
}

/// Resolve a `cred://…` reference through the credential store for the
/// calling security context.
async fn resolve_secret_raw(
    credstore: &dyn credstore_sdk::CredStoreClientV1,
    ctx: &RequestContext<'_>,
    secret_ref: &str,
) -> Result<String, AuthError> {
    let schema_key = secret_ref
        .strip_prefix("cred://")
        .unwrap_or(secret_ref)
        .to_owned();
    let reference = credstore_sdk::SecretRef::new(schema_key)
        .map_err(|_| AuthError::SecretNotFound(secret_ref.to_owned()))?;
    let resp = credstore
        .get(ctx.security, &reference)
        .await
        .map_err(|e| AuthError::Backend(format!("credstore lookup failed: {e}")))?;
    match resp {
        Some(secret) => String::from_utf8(secret.value.as_bytes().to_vec())
            .map_err(|_| AuthError::SecretNotFound(secret_ref.to_owned())),
        None => Err(AuthError::SecretNotFound(secret_ref.to_owned())),
    }
}

// ---------------------------------------------------------------------------
// required_headers guard
// ---------------------------------------------------------------------------

/// Guard plugin enforcing the presence of request/response headers
/// (ADR 0009). Config keys `required_request_headers` / `required_response_headers`
/// are comma-separated header names; a missing request header yields 400
/// (REQUIRED_HEADER_MISSING), a missing response header yields 502.
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    fn parse_list(config: &Value, key: &str) -> Vec<String> {
        config
            .get(key)
            .and_then(Value::as_str)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        gts::GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &'static str {
        gts::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext<'_>) -> Result<(), GuardError> {
        let required = Self::parse_list(ctx.config, "required_request_headers");
        if required.is_empty() {
            return Ok(()); // fail-open on absent/blank config
        }
        for name in required {
            let present = ctx
                .headers
                .get(&name)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !present {
                return Err(GuardError::Request {
                    error_code: "REQUIRED_HEADER_MISSING".to_owned(),
                    detail: format!("required request header '{name}' is missing"),
                });
            }
        }
        Ok(())
    }

    async fn guard_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), GuardError> {
        let required = Self::parse_list(ctx.config, "required_response_headers");
        if required.is_empty() {
            return Ok(()); // fail-open
        }
        for name in required {
            let present = ctx
                .headers
                .get(&name)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !present {
                return Err(GuardError::Response {
                    error_code: "REQUIRED_HEADER_MISSING".to_owned(),
                    detail: format!("required response header '{name}' is missing"),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// request_id transform
// ---------------------------------------------------------------------------

/// Transform plugin propagating/generating `X-Request-ID`.
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        gts::TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &'static str {
        gts::TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), TransformError> {
        let existing = ctx.inbound_header("x-request-id").map(str::to_owned);
        let value = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        ctx.set_outbound_header("x-request-id", value);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), TransformError> {
        if !ctx.headers.contains_key("x-request-id") {
            ctx.set_header("x-request-id", Uuid::new_v4().to_string());
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext<'_>) -> Result<(), TransformError> {
        Ok(())
    }
}
