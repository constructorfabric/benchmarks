//! Credential resolution and the credential-bearing built-in auth plugins.
//!
//! Two concerns live here:
//!
//! * [`CredentialStore`] — the single point through which a `cred://`
//!   reference becomes a secret value. The reference is passed **stripped**
//!   (`cred://api-key` → `api-key`) because `credstore-sdk`'s
//!   [`SecretRef`] validates `[a-zA-Z0-9_-]+`.
//! * [`ApiKeyAuthPlugin`] and [`OAuth2ClientCredAuthPlugin`] — the two
//!   credential-bearing built-ins (ADR `0002-plugin-system`, ADR 0008). The
//!   OAuth2 plugin keeps an internal `pingora-memory-cache` token cache keyed
//!   by `(tenant, subject, auth method, config hash)` and stores a
//!   key-carrying [`CachedToken`] so a `u64` hash collision can never hand
//!   one tenant another tenant's token.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::models::SecretRef;
use credstore_sdk::{CredStoreClientV1, GetSecretResponse};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AUTH_PLUGIN_API_KEY, AUTH_PLUGIN_OAUTH2_CLIENT_CRED, AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
    AuthPlugin, RequestContext,
};

/// Scheme prefix of a credstore reference.
const CRED_SCHEME: &str = "cred://";

/// Safety margin subtracted from an IdP-issued `expires_in` (ADR 0008).
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// `true` when `reference` is a `cred://` URI; the reference is passed to
/// credstore **without** the scheme either way.
#[must_use]
pub fn strip_cred_scheme(reference: &str) -> &str {
    let trimmed = reference.trim();
    trimmed
        .strip_prefix(CRED_SCHEME)
        .unwrap_or(trimmed)
        .trim_start_matches('/')
}

/// Parses `cred://api-key` (or a bare `api-key`) into a [`SecretRef`].
///
/// # Errors
///
/// [`DomainError::SecretNotFound`] when the reference is not a valid
/// credstore key (empty, too long, or holding a reserved character).
pub fn secret_ref(reference: &str) -> Result<SecretRef, DomainError> {
    SecretRef::new(strip_cred_scheme(reference))
        .map_err(|error| DomainError::SecretNotFound(format!("invalid secret reference: {error}")))
}

/// Extracts the secret text out of a credstore response.
fn secret_text(response: &GetSecretResponse) -> String {
    String::from_utf8_lossy(response.value.as_bytes()).into_owned()
}

/// Resolves `cred://` references through the credstore client.
///
/// The client is injected by the gear on start-up
/// (`ctx.client_hub().get::<dyn CredStoreClientV1>()`); until then every
/// lookup fails closed with [`DomainError::SecretNotFound`] — a request is
/// never forwarded with a credential it could not resolve.
pub struct CredentialStore {
    client: RwLock<Option<Arc<dyn CredStoreClientV1>>>,
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self {
            client: RwLock::new(None),
        }
    }
}

// The client handle is not `Debug`; the store never renders a secret anyway.
impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("client", &self.client.read().expect("credential store lock").is_some())
            .finish()
    }
}

impl CredentialStore {
    /// Empty store (no credstore client wired yet).
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Wires the credstore client.
    pub fn set_client(&self, client: Arc<dyn CredStoreClientV1>) {
        *self.client.write().expect("credential store lock") = Some(client);
    }

    /// The wired client, if any.
    #[must_use]
    pub fn client(&self) -> Option<Arc<dyn CredStoreClientV1>> {
        self.client.read().expect("credential store lock").clone()
    }

    /// Resolves one `cred://` reference for `ctx`.
    ///
    /// # Errors
    ///
    /// [`DomainError::SecretNotFound`] when no credstore client is wired, the
    /// reference is malformed or the secret is missing/inaccessible.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<String, DomainError> {
        let key = secret_ref(reference)?;
        let client = self.client().ok_or_else(|| {
            DomainError::SecretNotFound(format!(
                "credential store is not wired; cannot resolve '{reference}'"
            ))
        })?;
        match client.get(ctx, &key).await {
            Ok(Some(response)) => Ok(secret_text(&response)),
            Ok(None) => Err(DomainError::SecretNotFound(format!(
                "secret reference '{reference}' could not be resolved"
            ))),
            Err(error) => Err(DomainError::SecretNotFound(format!(
                "secret reference '{reference}' failed: {error}"
            ))),
        }
    }
}

/// The `plugins.configs[plugin_ref]` map of an upstream/route binding.
#[must_use]
pub fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

/// Alias of [`config_string`], for call sites that treat the value as a raw
/// (possibly blank) string.
use config_string as config_string_option;

/// Reads a `cred://` (or bare) reference from a plugin config object.
fn config_reference(config: &serde_json::Value, key: &str) -> Option<String> {
    config_string(config, key)
}

/// API-key auth plugin (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
///
/// Resolves the configured `cred://` reference and injects it into the
/// configured header. Configuration keys:
///
/// | Key | Required | Description |
/// |---|---|---|
/// | `api_key_ref` | yes | `cred://` reference holding the key |
/// | `header` | no | header to inject into (default `authorization`) |
/// | `prefix` | no | value prefix, e.g. `Bearer ` (default empty) |
pub struct ApiKeyAuthPlugin {
    credentials: Arc<CredentialStore>,
}

impl ApiKeyAuthPlugin {
    /// Default header the key is injected into.
    pub const DEFAULT_HEADER: &'static str = "authorization";

    /// Builds the plugin over a credential store.
    #[must_use]
    pub fn new(credentials: Arc<CredentialStore>) -> Self {
        Self { credentials }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "cf.core.oagw.apikey.v1"
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_API_KEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let config = ctx.plugin_config.clone().ok_or_else(|| {
            DomainError::Validation(format!(
                "auth plugin '{}' requires a config object with 'api_key_ref'",
                self.plugin_type()
            ))
        })?;
        let reference = config_reference(&config, "api_key_ref").ok_or_else(|| {
            DomainError::Validation(format!(
                "auth plugin '{}' requires an 'api_key_ref' credential reference",
                self.plugin_type()
            ))
        })?;
        let security = ctx.security.clone().ok_or_else(|| {
            DomainError::SecretNotFound("no caller identity on the proxied request".to_owned())
        })?;
        let api_key = self.credentials.resolve(&security, &reference).await?;

        let header = config_string(&config, "header")
            .unwrap_or_else(|| Self::DEFAULT_HEADER.to_owned());
        let prefix = config_string_option(&config, "prefix").unwrap_or_default();
        let name = http::HeaderName::try_from(header.as_str()).map_err(|_| {
            DomainError::Validation(format!("'{header}' is not a valid header name"))
        })?;
        let value = http::HeaderValue::try_from(format!("{prefix}{api_key}")).map_err(|_| {
            DomainError::Validation("the injected credential is not a valid header value".to_owned())
        })?;
        ctx.set_secret("api_key", api_key);
        ctx.inject_header(name, value);
        Ok(())
    }
}

/// A cached OAuth2 access token, carrying its own key.
///
/// `pingora-memory-cache` hashes keys to `u64` and does not resolve
/// collisions, so the key is verified on every hit (ADR 0008).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: Arc<toolkit_auth::oauth2::SecretString>,
}

/// OAuth2 client-credentials auth plugin (ADR 0008).
///
/// Two registered variants share this implementation and differ only in how
/// the client credentials reach the token endpoint:
///
/// | GTS plugin id | method |
/// |---|---|
/// | `…~cf.core.oagw.oauth2_client_cred.v1` | `Form` (body) |
/// | `…~cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` (header) |
///
/// Configuration keys (ADR 0008): `token_endpoint` *or* `issuer_url`,
/// `client_id_ref`, `client_secret_ref`, optional `scopes`.
pub struct OAuth2ClientCredAuthPlugin {
    credentials: Arc<CredentialStore>,
    method: ClientAuthMethod,
    /// `client_secret_post` when true, `client_secret_basic` otherwise.
    basic: bool,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the Form variant.
    #[must_use]
    pub fn client_secret_post(credentials: Arc<CredentialStore>, ttl: Duration, capacity: usize) -> Self {
        Self::new(credentials, false, ttl, capacity)
    }

    /// Builds the Basic variant.
    #[must_use]
    pub fn client_secret_basic(credentials: Arc<CredentialStore>, ttl: Duration, capacity: usize) -> Self {
        Self::new(credentials, true, ttl, capacity)
    }

    fn new(
        credentials: Arc<CredentialStore>,
        basic: bool,
        ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            credentials,
            method: if basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            basic,
            cache: MemoryCache::new(capacity.max(1)),
            cache_ttl: ttl,
        }
    }

    /// Cache key: `(tenant, subject, auth method, config hash)`.
    fn cache_key(&self, ctx: &RequestContext) -> Option<String> {
        let config = ctx.plugin_config.as_ref()?;
        let digest = config_digest(config);
        Some(format!(
            "{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.subject_id.unwrap_or(ctx.tenant_id),
            if self.basic { "basic" } else { "form" },
            digest
        ))
    }
}

/// Deterministic digest of a plugin config object.
///
/// The keys are sorted into a `BTreeMap` before serialising, so two
/// configurations that differ only in key order share one cache entry.
fn config_digest(config: &serde_json::Value) -> String {
    let Some(map) = config.as_object() else {
        return String::new();
    };
    let flattened: BTreeMap<&str, String> = map
        .iter()
        .map(|(key, value)| (key.as_str(), value.to_string()))
        .collect();
    serde_json::to_string(&flattened).unwrap_or_default()
}

/// Resolved OAuth2 plugin configuration.
#[derive(Debug)]
struct OAuth2PluginConfig {
    token_endpoint: Option<url::Url>,
    issuer_url: Option<url::Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    /// Parses the plugin config object.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] for a missing or mutually-exclusive key.
    fn parse(config: &serde_json::Value) -> Result<Self, DomainError> {
        let token_endpoint = match config_string(config, "token_endpoint") {
            Some(raw) => Some(url::Url::parse(&raw).map_err(|error| {
                DomainError::Validation(format!("auth config 'token_endpoint' is not a URL: {error}"))
            })?),
            None => None,
        };
        let issuer_url = match config_string(config, "issuer_url") {
            Some(raw) => Some(url::Url::parse(&raw).map_err(|error| {
                DomainError::Validation(format!("auth config 'issuer_url' is not a URL: {error}"))
            })?),
            None => None,
        };
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(DomainError::Validation(
                "auth config accepts either 'token_endpoint' or 'issuer_url', not both".to_owned(),
            ));
        }
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(DomainError::Validation(
                "auth config requires 'token_endpoint' or 'issuer_url'".to_owned(),
            ));
        }
        let client_id_ref = config_reference(config, "client_id_ref")
            .ok_or_else(|| DomainError::Validation("auth config requires 'client_id_ref'".to_owned()))?;
        let client_secret_ref = config_reference(config, "client_secret_ref").ok_or_else(|| {
            DomainError::Validation("auth config requires 'client_secret_ref'".to_owned())
        })?;
        let scopes = config_string_option(config, "scopes")
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
        if self.basic {
            AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC
        } else {
            AUTH_PLUGIN_OAUTH2_CLIENT_CRED
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let config = ctx.plugin_config.clone().ok_or_else(|| {
            DomainError::Validation(format!(
                "auth plugin '{}' requires a config object",
                self.plugin_type()
            ))
        })?;
        let settings = OAuth2PluginConfig::parse(&config)?;
        let key = self
            .cache_key(ctx)
            .ok_or_else(|| DomainError::Validation("auth config is not an object".to_owned()))?;

        // 1. Cached token for this (tenant, subject, method, config)?
        // A key mismatch is a hash collision, treated as a miss (ADR 0008).
        if let Some(entry) = self.cache.get(&key).0
            && entry.key == key
        {
            self.inject_bearer(ctx, entry.token.expose());
            return Ok(());
        }

        // 2. Resolve both credentials through credstore.
        let security = ctx.security.clone().ok_or_else(|| {
            DomainError::SecretNotFound("no caller identity on the proxied request".to_owned())
        })?;
        let client_id = self
            .credentials
            .resolve(&security, &settings.client_id_ref)
            .await?;
        let client_secret = self
            .credentials
            .resolve(&security, &settings.client_secret_ref)
            .await?;

        // 3. Exchange them for an access token.
        let fetched = fetch_token(OAuthClientConfig {
            token_endpoint: settings.token_endpoint,
            issuer_url: settings.issuer_url,
            client_id,
            client_secret: toolkit_auth::oauth2::SecretString::new(client_secret),
            scopes: settings.scopes,
            auth_method: self.method,
            ..OAuthClientConfig::default()
        })
        .await
        .map_err(|error| DomainError::AuthenticationFailed(error.to_string()))?;

        // 4. Cache for `min(config_ttl, expires_in − 30s)`; failed fetches are
        //    never cached, so the next request retries the IdP.
        let ttl = self
            .cache_ttl
            .min(fetched.expires_in.saturating_sub(TOKEN_EXPIRY_MARGIN));
        let authorization = format!("Bearer {}", fetched.bearer.expose());
        if !ttl.is_zero() {
            self.cache.put(
                &key.clone(),
                CachedToken {
                    key,
                    token: Arc::new(fetched.bearer),
                },
                Some(ttl),
            );
        }

        self.inject_bearer(ctx, &authorization);
        Ok(())
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Injects an `Authorization` header value.
    fn inject_bearer(&self, ctx: &mut RequestContext, authorization: &str) {
        if let Ok(value) = http::HeaderValue::try_from(authorization.to_owned()) {
            ctx.inject_header(http::header::AUTHORIZATION, value);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::plugin::RequestContext;
    use uuid::Uuid;

    fn ctx() -> RequestContext {
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        ctx.security = Some(Arc::new(
            SecurityContext::builder()
                .subject_id(Uuid::now_v7())
                .subject_tenant_id(Uuid::nil())
                .build()
                .expect("context"),
        ));
        ctx
    }

    #[test]
    fn cred_scheme_is_stripped() {
        assert_eq!(strip_cred_scheme("cred://api-key"), "api-key");
        assert_eq!(strip_cred_scheme("cred://nested/key"), "nested/key");
        assert_eq!(strip_cred_scheme("api-key"), "api-key");
        assert_eq!(strip_cred_scheme("  cred://api-key  "), "api-key");
    }

    #[test]
    fn invalid_references_fail_closed() {
        let error = secret_ref("cred://has spaces").unwrap_err();
        assert_eq!(error.http_status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[test]
    fn oauth2_config_is_validated() {
        let error = OAuth2PluginConfig::parse(&serde_json::json!({})).unwrap_err();
        assert_eq!(error.http_status(), 400);

        let error = OAuth2PluginConfig::parse(&serde_json::json!({
            "token_endpoint": "https://idp.example/token",
            "issuer_url": "https://idp.example",
            "client_id_ref": "cred://a",
            "client_secret_ref": "cred://b",
        }))
        .unwrap_err();
        assert_eq!(error.http_status(), 400);

        let parsed = OAuth2PluginConfig::parse(&serde_json::json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://a",
            "client_secret_ref": "cred://b",
            "scopes": "read write",
        }))
        .expect("parsed");
        assert_eq!(parsed.scopes, vec!["read".to_owned(), "write".to_owned()]);
    }

    #[tokio::test]
    async fn api_key_plugin_requires_its_config() {
        let plugin = ApiKeyAuthPlugin::new(CredentialStore::new());
        let mut ctx = ctx();
        let error = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert_eq!(error.http_status(), 400);
    }

    #[tokio::test]
    async fn api_key_plugin_fails_closed_without_a_credstore() {
        let plugin = ApiKeyAuthPlugin::new(CredentialStore::new());
        let mut ctx = ctx();
        ctx.plugin_config = Some(serde_json::json!({ "api_key_ref": "cred://api-key" }));
        let error = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert_eq!(error.http_status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn oauth2_plugin_without_a_caller_identity_fails() {
        let plugin = OAuth2ClientCredAuthPlugin::client_secret_post(
            CredentialStore::new(),
            Duration::from_secs(300),
            8,
        );
        let mut ctx = ctx();
        ctx.plugin_config = Some(serde_json::json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://id",
            "client_secret_ref": "cred://secret",
        }));
        ctx.security = None;
        let error = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert_eq!(error.http_status(), 500);
    }

    #[test]
    fn config_digest_is_order_insensitive() {
        let a = serde_json::json!({ "a": 1, "b": "x" });
        let b = serde_json::json!({ "b": "x", "a": 1 });
        assert_eq!(config_digest(&a), config_digest(&b));
        assert_ne!(
            config_digest(&a),
            config_digest(&serde_json::json!({ "a": 2, "b": "x" }))
        );
    }

    #[test]
    fn cache_keys_isolate_tenants_subjects_and_methods() {
        let form = OAuth2ClientCredAuthPlugin::client_secret_post(
            CredentialStore::new(),
            Duration::from_secs(300),
            8,
        );
        let basic = OAuth2ClientCredAuthPlugin::client_secret_basic(
            CredentialStore::new(),
            Duration::from_secs(300),
            8,
        );
        let mut ctx = ctx();
        ctx.plugin_config = Some(serde_json::json!({ "token_endpoint": "https://idp/token" }));
        let form_key = form.cache_key(&ctx).expect("key");
        let basic_key = basic.cache_key(&ctx).expect("key");
        assert_ne!(form_key, basic_key);

        ctx.subject_id = Some(Uuid::now_v7());
        assert_ne!(form.cache_key(&ctx).expect("key"), form_key);
    }

    #[tokio::test]
    async fn cached_tokens_are_key_verified() {
        let plugin = OAuth2ClientCredAuthPlugin::client_secret_post(
            CredentialStore::new(),
            Duration::from_secs(300),
            8,
        );
        let key = "tenant:subject:form:config".to_owned();
        plugin.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                token: Arc::new(toolkit_auth::oauth2::SecretString::new("token-1")),
            },
            Some(Duration::from_secs(60)),
        );
        let (hit, _status) = plugin.cache.get(&key);
        assert!(hit.is_some_and(|entry| entry.key == key));
        // A colliding entry under a different key is a miss.
        let (other, _status) = plugin.cache.get("other");
        assert!(other.is_none());
    }
}
