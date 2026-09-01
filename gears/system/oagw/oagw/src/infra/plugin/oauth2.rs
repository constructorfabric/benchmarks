//! Built-in OAuth2 client-credentials auth plugin (ADR-0008), registered twice:
//!
//! | GTS plugin id | Client auth method |
//! |---|---|
//! | `...auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | `Form` (credentials in the request body) |
//! | `...auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` (credentials in the `Authorization` header) |
//!
//! ## Configuration keys
//!
//! | Key | Required | Meaning |
//! |---|---|---|
//! | `token_endpoint` | XOR `issuer_url` | Direct token endpoint URL. |
//! | `issuer_url` | XOR `token_endpoint` | OIDC issuer URL; the token endpoint is discovered. |
//! | `client_id_ref` | yes | `cred://` reference for the client id. |
//! | `client_secret_ref` | yes | `cred://` reference for the client secret. |
//! | `scopes` | no | Space-separated OAuth2 scopes. |
//!
//! ## Token cache
//!
//! * TTL = `min(configured ttl, expires_in - 30s safety margin)`;
//! * tokens with `expires_in <= 30s` are **not** cached;
//! * key = `{tenant}:{subject}:{auth_method_tag}:{config_hash}`, with the
//!   original key verified on every hit so a hash collision can never hand out
//!   another tenant's token;
//! * failed fetches are **never** cached — the next request retries the IdP;
//! * the default TTL and the capacity come from
//!   [`OagwConfig::token_cache_ttl_secs`] / [`OagwConfig::token_cache_capacity`].
//!
//! ## Failure mapping
//!
//! ADR-0008 reports both credential-store and IdP failures as an internal
//! plugin error without pinning an HTTP status, so this module maps them onto
//! the gear's own taxonomy:
//!
//! | Failure | Status | GTS type |
//! |---|---|---|
//! | Malformed plugin configuration | 400 | `cf.oagw.validation.error.v1` |
//! | Credential store unreachable / failing | 500 | `cf.oagw.secret.not_found.v1` |
//! | Reference not accessible to the tenant | 401 | `cf.oagw.auth.failed.v1` |
//! | Token endpoint request failed | 502 | `cf.oagw.downstream.error.v1` |
//!
//! ## Deviation from ADR-0008 (documented)
//!
//! The ADR stores `SecretString` in the cache; the in-memory cache this gear
//! uses requires `Clone`, which `SecretString` does not implement, so the
//! cached token is a plain `String` whose `Debug` impl is redacted. The value
//! is still never logged, and the cache holds only bearer tokens (not client
//! secrets, which stay inside the fetch call).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use pingora_memory_cache::MemoryCache;
use serde::Deserialize;
use toolkit_auth::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};
use crate::infra::plugin::secret::SecretResolver;

/// Safety margin subtracted from the IdP-reported `expires_in` (ADR-0008).
pub const TOKEN_EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// `Authorization` header the bearer token is injected into.
pub const AUTHORIZATION_HEADER: &str = "authorization";

/// Tag of the `Form` client-auth method in the cache key.
pub const FORM_AUTH_METHOD_TAG: &str = "form";

/// Tag of the `Basic` client-auth method in the cache key.
pub const BASIC_AUTH_METHOD_TAG: &str = "basic";

/// Cache configuration handed down from [`OagwConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Upper bound for a cached access token TTL.
    pub ttl: Duration,
    /// Maximum number of cached tokens.
    pub capacity: usize,
}

impl From<&OagwConfig> for TokenCacheConfig {
    fn from(config: &OagwConfig) -> Self {
        Self {
            ttl: Duration::from_secs(config.token_cache_ttl_secs),
            capacity: config.token_cache_capacity,
        }
    }
}

/// Configuration payload of the OAuth2 plugin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuth2PluginConfig {
    /// Direct token endpoint URL; mutually exclusive with `issuer_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_endpoint: Option<String>,
    /// OIDC issuer URL; mutually exclusive with `token_endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer_url: Option<String>,
    /// `cred://` reference for the client id.
    client_id_ref: String,
    /// `cred://` reference for the client secret.
    client_secret_ref: String,
    /// Space-separated OAuth2 scopes.
    #[serde(default)]
    scopes: Option<String>,
}

/// A cached bearer token, carrying the key it was stored under so a hash
/// collision degrades to a miss instead of another tenant's token (ADR-0008).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<SecretString>,
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

/// OAuth2 client credentials plugin.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    auth_method: ClientAuthMethod,
    config: OAuth2PluginConfig,
    /// Deterministic fingerprint of the binding configuration (cache key).
    config_hash: String,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
    http_config: Option<toolkit_http::HttpClientConfig>,
}

// The resolver port and the token cache are not `Debug`, so the plugin reports
// only its identity and configuration fingerprint.
impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuth2ClientCredAuthPlugin")
            .field("id", &self.id())
            .field("auth_method", &self.auth_method)
            .field("config_hash", &self.config_hash)
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Registry key of the `Form` variant.
    pub const FORM_PLUGIN_ID: &'static str = builtin::OAUTH2_CLIENT_CRED;

    /// Registry key of the `Basic` variant.
    pub const BASIC_PLUGIN_ID: &'static str = builtin::OAUTH2_CLIENT_CRED_BASIC;

    /// GTS base type of this plugin.
    pub const PLUGIN_TYPE: &'static str = AUTH_PLUGIN_TYPE_ID;

    /// Builds one plugin variant from a binding configuration payload.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the payload does not match the
    /// documented keys, when `token_endpoint` and `issuer_url` are both set or
    /// both missing, when a reference is not a `cred://` URI, or when the
    /// token endpoint is not a valid URL.
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        auth_method: ClientAuthMethod,
        cache_config: TokenCacheConfig,
        config: &serde_json::Value,
    ) -> Result<Self, OagwError> {
        let parsed = parse_config(config)?;
        validate_endpoints(&parsed)?;
        validate_references(&parsed)?;
        let cache = MemoryCache::new(cache_config.capacity.max(1));
        Ok(Self {
            resolver,
            auth_method,
            config: parsed,
            config_hash: config_fingerprint(config),
            cache,
            cache_ttl: cache_config.ttl,
            http_config: None,
        })
    }

    /// Overrides the HTTP client configuration used for the token exchange
    /// (and for OIDC discovery). Tests install a plaintext-tolerant preset.
    pub fn set_http_config(&mut self, http_config: toolkit_http::HttpClientConfig) {
        self.http_config = Some(http_config);
    }

    /// Cache key for one `(tenant, subject, auth method, config)` tuple.
    #[must_use]
    pub fn cache_key(&self, ctx: &RequestContext) -> String {
        format!(
            "{}:{}:{}:{}",
            tenant_of(ctx),
            subject_of(ctx),
            auth_method_tag(self.auth_method),
            self.config_hash
        )
    }

    /// Deterministic fingerprint of the binding configuration.
    #[must_use]
    pub fn config_fingerprint(&self) -> String {
        self.config_hash.clone()
    }

    fn lookup(&self, key: &str) -> Option<Arc<SecretString>> {
        let (entry, _status) = self.cache.get(key);
        let entry = entry?;
        if entry.key != key {
            return None;
        }
        Some(entry.token)
    }

    fn store(&self, key: &str, token: SecretString, ttl: Duration) {
        self.cache.put(
            &key.to_owned(),
            CachedToken {
                key: key.to_owned(),
                token: Arc::new(token),
            },
            Some(ttl),
        );
    }

    async fn resolve(
        &self,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<String, OagwError> {
        self.resolver
            .resolve(security, reference)
            .await?
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                OagwError::authentication_failed(format!(
                    "oauth2 credential '{reference}' is not accessible to this tenant"
                ))
            })
    }

    async fn fetch_bearer_token(
        &self,
        security: &SecurityContext,
    ) -> Result<(String, Duration), OagwError> {
        let client_id = self.resolve(security, &self.config.client_id_ref).await?;
        let client_secret = self
            .resolve(security, &self.config.client_secret_ref)
            .await?;
        let client_config = OAuthClientConfig {
            token_endpoint: self
                .parsed_url("token_endpoint", self.config.token_endpoint.as_ref())?,
            issuer_url: self.parsed_url("issuer_url", self.config.issuer_url.as_ref())?,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: self.parsed_scopes(),
            auth_method: self.auth_method,
            http_config: self.http_config.clone(),
            ..OAuthClientConfig::default()
        };
        client_config.validate().map_err(|error| {
            OagwError::validation(format!("invalid oauth2 plugin configuration: {error}"))
        })?;
        let fetched = fetch_token(client_config).await.map_err(|error| {
            OagwError::downstream_error(format!("oauth2 token endpoint request failed: {error}"))
        })?;
        Ok((fetched.bearer.expose().to_owned(), fetched.expires_in))
    }

    fn parsed_url(&self, field: &str, value: Option<&String>) -> Result<Option<Url>, OagwError> {
        value
            .map(|raw| {
                Url::parse(raw).map_err(|error| {
                    OagwError::validation(format!("invalid oauth2 {field} '{raw}': {error}"))
                })
            })
            .transpose()
    }

    fn parsed_scopes(&self) -> Vec<String> {
        self.config
            .scopes
            .as_deref()
            .map(|scopes| scopes.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Cache TTL for a token that expires in `expires_in`, or `None` when the
    /// token must not be cached (ADR-0008).
    #[must_use]
    pub fn cache_ttl_for(&self, expires_in: Duration) -> Option<Duration> {
        if expires_in <= TOKEN_EXPIRY_SAFETY_MARGIN {
            return None;
        }
        let ttl = expires_in
            .checked_sub(TOKEN_EXPIRY_SAFETY_MARGIN)
            .unwrap_or_default();
        Some(self.cache_ttl.min(ttl)).filter(|ttl| !ttl.is_zero())
    }
}

fn parse_config(config: &serde_json::Value) -> Result<OAuth2PluginConfig, OagwError> {
    let payload = match config {
        serde_json::Value::Null => &serde_json::Value::Object(serde_json::Map::new()),
        value => value,
    };
    serde_json::from_value(payload.clone())
        .map_err(|error| OagwError::validation(format!("invalid oauth2 plugin config: {error}")))
}

fn validate_endpoints(config: &OAuth2PluginConfig) -> Result<(), OagwError> {
    match (config.token_endpoint.as_ref(), config.issuer_url.as_ref()) {
        (Some(_), Some(_)) => Err(OagwError::validation(
            "oauth2 plugin accepts either `token_endpoint` or `issuer_url`, not both",
        )),
        (None, None) => Err(OagwError::validation(
            "oauth2 plugin requires one of `token_endpoint` or `issuer_url`",
        )),
        _ => Ok(()),
    }
}

fn validate_references(config: &OAuth2PluginConfig) -> Result<(), OagwError> {
    for (field, reference) in [
        ("client_id_ref", &config.client_id_ref),
        ("client_secret_ref", &config.client_secret_ref),
    ] {
        if !reference.starts_with(crate::infra::plugin::secret::SECRET_REF_SCHEME) {
            return Err(OagwError::validation(format!(
                "oauth2 plugin field `{field}` must be a '{scheme}' reference, got '{reference}'",
                scheme = crate::infra::plugin::secret::SECRET_REF_SCHEME
            )));
        }
    }
    Ok(())
}

/// Deterministic, order-independent fingerprint of the binding configuration.
///
/// `serde_json::Map` is a `BTreeMap` in this workspace, so the canonical
/// serialisation is already key-sorted; the value is hashed to keep cache keys
/// short.
#[must_use]
fn config_fingerprint(config: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(config).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Tag an auth method contributes to the cache key (ADR-0008).
#[must_use]
pub const fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => FORM_AUTH_METHOD_TAG,
        ClientAuthMethod::Basic => BASIC_AUTH_METHOD_TAG,
    }
}

/// Tenant of the request, preferring the security context.
fn tenant_of(ctx: &RequestContext) -> uuid::Uuid {
    ctx.security
        .as_ref()
        .map(|security| security.subject_tenant_id())
        .unwrap_or(ctx.tenant_id)
}

/// Subject of the request, preferring the security context.
fn subject_of(ctx: &RequestContext) -> uuid::Uuid {
    ctx.security
        .as_ref()
        .map(|security| security.subject_id())
        .or(ctx.subject_id)
        .unwrap_or_default()
}

fn inject_bearer(ctx: &mut RequestContext, token: &str) -> Result<(), OagwError> {
    let value = format!("Bearer {token}");
    let header_value = HeaderValue::from_str(&value)
        .map_err(|_| OagwError::validation("oauth2 bearer token is not a valid header value"))?;
    let header_name = HeaderName::from_static(AUTHORIZATION_HEADER);
    ctx.headers
        .insert(header_name.clone(), header_value.clone());
    ctx.inject_header(header_name, header_value);
    Ok(())
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => builtin::OAUTH2_CLIENT_CRED,
            ClientAuthMethod::Basic => builtin::OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let key = self.cache_key(ctx);
        if let Some(token) = self.lookup(&key) {
            inject_bearer(ctx, token.expose())?;
            return Ok(());
        }
        let Some(security) = ctx.security.clone() else {
            return Err(OagwError::authentication_failed(
                "no security context is available to resolve the oauth2 credentials",
            ));
        };
        let (token, expires_in) = self.fetch_bearer_token(&security).await?;
        if let Some(ttl) = self.cache_ttl_for(expires_in) {
            self.store(&key, SecretString::new(token.clone()), ttl);
        }
        inject_bearer(ctx, &token)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "oauth2_tests.rs"]
mod tests;
