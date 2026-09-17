//! Builtin auth plugins (ADR 0002, 0008).
//!
//! * [`NoopAuthPlugin`] — no authentication (default; the `noop.v1`
//!   instance).
//! * [`ApiKeyAuthPlugin`] — inject a `cred://`-referenced static secret
//!   into a configurable header (`apikey.v1`).
//! * [`OAuth2ClientCredAuthPlugin`] — `OAuth2` client-credentials exchange
//!   against a token endpoint, with an internal TTL token cache keyed by
//!   `tenant:subject:auth_method:config_hash` and verified on hit
//!   (ADR 0008). Registered twice: `oauth2_client_cred.v1` (Form) and
//!   `oauth2_client_cred_basic.v1` (Basic client auth).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use http::header::HeaderMap;
use toolkit_auth::oauth2;
use toolkit_auth::oauth2::SecretString;
use url::Url;

use crate::domain::plugin::{AuthPlugin, PluginContext, PluginError};
use crate::gts_helpers;
use credstore_sdk::SecretRef;

/// Resolve a `cred://` reference (or bare key) to its plaintext value.
///
/// Secrets resolve under the proxying principal's identity so hierarchical
/// sharing works unchanged.
pub(crate) async fn resolve_secret(
    ctx: &PluginContext,
    reference: &str,
) -> Result<String, PluginError> {
    let key = reference.strip_prefix("cred://").unwrap_or(reference);
    let secret_ref = SecretRef::new(key).map_err(|e| {
        PluginError::Internal(format!("invalid secret reference '{key}': {e}"))
    })?;
    let resp = ctx
        .cred_store
        .get(&ctx.security_context, &secret_ref)
        .await
        .map_err(|e| {
            PluginError::Internal(format!("credential store lookup for '{key}' failed: {e}"))
        })?;
    let Some(resp) = resp else {
        return Err(PluginError::SecretNotFound(format!(
            "secret '{key}' does not exist"
        )));
    };
    Ok(String::from_utf8_lossy(resp.value.as_bytes()).into_owned())
}

/// Auth — no authentication. The default binding for upstreams without an
/// `auth` block.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::AUTH_PLUGIN_NOOP
    }

    fn plugin_type(&self) -> &'static str {
        "auth"
    }

    async fn authenticate(
        &self,
        _ctx: &PluginContext,
        _headers: &mut HeaderMap,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Auth — static API key injection.
///
/// Config:
/// * `secret_ref` (required) — `cred://` reference to the key value.
/// * `header` (default `Authorization`) — header the key is injected into.
///
/// The raw secret value is injected verbatim; operators embed any required
/// prefix (e.g. `"Bearer <key>"`) in the stored secret.
pub struct ApiKeyAuthPlugin;

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::AUTH_PLUGIN_APIKEY
    }

    fn plugin_type(&self) -> &'static str {
        "auth"
    }

    async fn authenticate(
        &self,
        ctx: &PluginContext,
        headers: &mut HeaderMap,
    ) -> Result<(), PluginError> {
        let Some(secret_ref) = ctx.config.get("secret_ref").and_then(|v| v.as_str()) else {
            return Err(PluginError::Internal(
                "apikey plugin requires a 'secret_ref' config key".to_owned(),
            ));
        };
        let name = ctx
            .config
            .get("header")
            .and_then(|v| v.as_str())
            .unwrap_or("Authorization");
        let name = http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            PluginError::Internal(format!("apikey plugin: invalid header name '{name}': {e}"))
        })?;
        let value = resolve_secret(ctx, secret_ref).await?;
        let value = http::header::HeaderValue::from_str(&value).map_err(|e| {
            PluginError::Internal(format!("apikey plugin: invalid header value: {e}"))
        })?;
        headers.insert(name, value);
        Ok(())
    }
}

/// Parsed configuration for the `OAuth2` client-credentials plugin.
#[derive(Debug, Clone)]
struct OAuth2PluginConfig {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
}

impl OAuth2PluginConfig {
    /// Parse from the binding-site `config` object (ADR 0008 key names).
    fn from_value(config: &serde_json::Value) -> Result<Self, PluginError> {
        let token_endpoint = config
            .get("token_endpoint")
            .and_then(|v| v.as_str())
            .map(Url::parse)
            .transpose()
            .map_err(|e| PluginError::Internal(format!("invalid token_endpoint: {e}")))?;
        let issuer_url = config
            .get("issuer_url")
            .and_then(|v| v.as_str())
            .map(Url::parse)
            .transpose()
            .map_err(|e| PluginError::Internal(format!("invalid issuer_url: {e}")))?;
        let client_id_ref = required_str(config, "client_id_ref")?;
        let client_secret_ref = required_str(config, "client_secret_ref")?;
        let scopes = config
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::Internal(
                "oauth2 plugin requires exactly one of 'token_endpoint' / 'issuer_url'".to_owned(),
            ));
        }
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
        })
    }

    /// Deterministic hash over the identity-bearing config components
    /// (client refs + scopes + endpoint), used in the cache key.
    fn config_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        let endpoint = self
            .token_endpoint
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| self.issuer_url.as_ref().map(ToString::to_string));
        (&endpoint, &self.client_id_ref, &self.client_secret_ref, &self.scopes).hash(&mut h);
        h.finish()
    }
}

fn required_str(config: &serde_json::Value, key: &str) -> Result<String, PluginError> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::Internal(format!("oauth2 plugin requires '{key}'")))
}

/// One cached token: stores the cache key for collision verification plus
/// the token and its absolute expiry (ADR 0008 hash-collision safety).
struct CachedToken {
    key: String,
    token: SecretString,
    expires_at: Instant,
}

/// Auth — `OAuth2` client credentials, `Form` or `Basic` client auth.
///
/// Fetches a token from the token endpoint on cache miss (per unique
/// `tenant:subject:auth_method:config_hash`) and serves it for
/// `min(config_ttl, expires_in - 30s)`; failed fetches are not cached.
pub struct OAuth2ClientCredAuthPlugin {
    auth_method: oauth2::ClientAuthMethod,
    cache: DashMap<String, CachedToken>,
    cache_capacity: usize,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin with the given token-cache settings.
    ///
    /// Secrets resolve through the [`PluginContext`] cred store at
    /// token-fetch time, so no store is held here.
    #[must_use]
    pub fn new(
        auth_method: oauth2::ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            auth_method,
            cache: DashMap::new(),
            cache_capacity,
            cache_ttl,
        }
    }

    fn cache_key(&self, ctx: &PluginContext, config: &OAuth2PluginConfig) -> String {
        let sc = &ctx.security_context;
        format!(
            "{}:{}:{}:{}",
            sc.subject_tenant_id(),
            sc.subject_id(),
            auth_method_tag(self.auth_method),
            config.config_hash()
        )
    }

    fn cached(&self, key: &str) -> Option<SecretString> {
        let entry = self.cache.get(key)?;
        if entry.key != key || Instant::now() >= entry.expires_at {
            return None;
        }
        Some(entry.token.clone())
    }

    fn store(&self, key: String, token: SecretString, ttl: Duration) {
        if ttl.is_zero() {
            return; // do not cache near-expiry tokens (ADR 0008)
        }
        if !self.cache.contains_key(&key) && self.cache.len() >= self.cache_capacity {
            // Bound the cache: evict one arbitrary (oldest-inserted) entry.
            let victim = self.cache.iter().next().map(|r| r.key().clone());
            if let Some(victim) = victim {
                self.cache.remove(&victim);
            }
        }
        self.cache.insert(
            key.clone(),
            CachedToken {
                key,
                token,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Fetch a fresh token and cache it per ADR 0008.
    async fn fetch_and_cache(
        &self,
        ctx: &PluginContext,
        config: &OAuth2PluginConfig,
        cache_key: &str,
    ) -> Result<SecretString, PluginError> {
        let client_id = resolve_secret(ctx, &config.client_id_ref).await?;
        let client_secret = resolve_secret(ctx, &config.client_secret_ref).await?;
        let oauth_config = oauth2::OAuthClientConfig {
            token_endpoint: config.token_endpoint.clone(),
            issuer_url: config.issuer_url.clone(),
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: config.scopes.clone(),
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_mins(30),
            jitter_max: Duration::from_mins(5),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: self.cache_ttl,
            http_config: None,
        };
        let fetched = oauth2::fetch_token(oauth_config)
            .await
            .map_err(|e| PluginError::TokenAcquisitionFailed(format!("{e}")))?;
        let ttl = effective_ttl(fetched.expires_in, self.cache_ttl);
        self.store(cache_key.to_owned(), fetched.bearer.clone(), ttl);
        Ok(fetched.bearer)
    }
}

/// `min(config_ttl, expires_in - 30s)`; `Duration::ZERO` when the margin
/// would be negative (token not cached).
fn effective_ttl(expires_in: Duration, config_ttl: Duration) -> Duration {
    let margin = expires_in.saturating_sub(Duration::from_secs(30));
    config_ttl.min(margin)
}

fn auth_method_tag(method: oauth2::ClientAuthMethod) -> &'static str {
    match method {
        oauth2::ClientAuthMethod::Form => "form",
        oauth2::ClientAuthMethod::Basic => "basic",
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        match self.auth_method {
            oauth2::ClientAuthMethod::Form => gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
            oauth2::ClientAuthMethod::Basic => gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    fn plugin_type(&self) -> &'static str {
        "auth"
    }

    async fn authenticate(
        &self,
        ctx: &PluginContext,
        headers: &mut HeaderMap,
    ) -> Result<(), PluginError> {
        let config = OAuth2PluginConfig::from_value(&ctx.config)?;
        let cache_key = self.cache_key(ctx, &config);
        if let Some(token) = self.cached(&cache_key) {
            inject_bearer(headers, &token)?;
            return Ok(());
        }
        let token = self.fetch_and_cache(ctx, &config, &cache_key).await?;
        inject_bearer(headers, &token)
    }
}

/// Inject `Authorization: Bearer <token>` into the outbound headers.
fn inject_bearer(headers: &mut HeaderMap, token: &SecretString) -> Result<(), PluginError> {
    let value = format!("Bearer {}", token.expose());
    let value = http::header::HeaderValue::from_str(&value).map_err(|e| {
        PluginError::Internal(format!("oauth2 plugin: invalid bearer value: {e}"))
    })?;
    headers.insert(http::header::AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Arc;

    use credstore_sdk::{CredStoreClientV1, CredStoreError, GetSecretResponse, SecretValue, SharingMode, TenantId};
    use serde_json::json;
    use toolkit_security::SecurityContext;

    fn ctx(config: serde_json::Value) -> PluginContext {
        static CLIENT: std::sync::OnceLock<toolkit_http::HttpClient> = std::sync::OnceLock::new();
        let http = CLIENT
            .get_or_init(|| toolkit_http::HttpClient::new().expect("test http client"))
            .clone();
        PluginContext {
            security_context: SecurityContext::anonymous(),
            cred_store: Arc::new(TestCredStore),
            http,
            config,
        }
    }

    struct TestCredStore;
    #[async_trait]
    impl CredStoreClientV1 for TestCredStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            key: &SecretRef,
        ) -> Result<Option<GetSecretResponse>, CredStoreError> {
            Ok((key.as_ref() == "openai-key").then(|| GetSecretResponse {
                value: SecretValue::new(b"sk-secret".to_vec()),
                id: uuid::Uuid::new_v4(),
                owner_tenant_id: TenantId(uuid::Uuid::new_v4()),
                sharing: SharingMode::Private,
                is_inherited: false,
                version: 1,
                secret_type: "cf.core.credstore.secret.v1~cf.core.credstore.opaque.v1".into(),
                expires_at: None,
            }))
        }
    }

    #[tokio::test]
    async fn noop_injects_nothing() {
        let mut headers = HeaderMap::new();
        NoopAuthPlugin
            .authenticate(&ctx(json!({})), &mut headers)
            .await
            .unwrap();
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn apikey_injects_resolved_secret() {
        let mut headers = HeaderMap::new();
        ApiKeyAuthPlugin
            .authenticate(
                &ctx(json!({ "header": "Authorization", "secret_ref": "openai-key" })),
                &mut headers,
            )
            .await
            .unwrap();
        assert_eq!(
            headers.get("authorization").map(|v| v.to_str().unwrap()),
            Some("sk-secret")
        );
    }

    #[tokio::test]
    async fn apikey_missing_config_rejects() {
        let mut headers = HeaderMap::new();
        let err = ApiKeyAuthPlugin
            .authenticate(&ctx(json!({})), &mut headers)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("secret_ref"));
    }

    #[test]
    fn oauth2_config_rejects_both_and_neither_endpoints() {
        let both = OAuth2PluginConfig::from_value(&json!({
            "token_endpoint": "https://issuer/token",
            "issuer_url": "https://issuer",
            "client_id_ref": "c",
            "client_secret_ref": "s",
        }));
        assert!(both.is_err());
        let neither = OAuth2PluginConfig::from_value(&json!({
            "client_id_ref": "c",
            "client_secret_ref": "s",
        }));
        assert!(neither.is_err());
    }

    #[test]
    fn oauth2_effective_ttl_respects_safety_margin() {
        // expires 45s away, config ceiling 300s → cached for 15s.
        assert_eq!(
            effective_ttl(Duration::from_secs(45), Duration::from_mins(5)),
            Duration::from_secs(15)
        );
        // expires within 30s → not cached.
        assert_eq!(
            effective_ttl(Duration::from_secs(20), Duration::from_mins(5)),
            Duration::ZERO
        );
        // config ceiling smaller → wins.
        assert_eq!(
            effective_ttl(Duration::from_hours(1), Duration::from_mins(1)),
            Duration::from_mins(1)
        );
    }

    #[test]
    fn cache_keys_isolate_tenants_subjects_and_methods() {
        let a = oauth2::ClientAuthMethod::Form;
        let b = oauth2::ClientAuthMethod::Basic;
        assert_ne!(auth_method_tag(a), auth_method_tag(b));

        let cfg = OAuth2PluginConfig::from_value(&json!({
            "token_endpoint": "https://issuer/token",
            "client_id_ref": "cid",
            "client_secret_ref": "csec",
            "scopes": "read write"
        }))
        .unwrap();
        // Same config hashes deterministically.
        assert_eq!(cfg.config_hash(), cfg.config_hash());
    }
}
