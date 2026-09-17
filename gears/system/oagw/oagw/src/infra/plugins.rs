//! Built-in plugin registry and implementations (ADR-0002, ADR-0008,
//! ADR-0009).
//!
//! Resolvable plugin identifiers:
//! - Auth: `noop`, `apikey`, `oauth2_client_cred` (Form),
//!   `oauth2_client_cred_basic` (Basic).
//! - Guard: `required_headers`.
//! - Transform: `request_id`.
//!
//! `basic`/`bearer` (auth) and `timeout`/`cors` (guard) and
//! `logging`/`metrics` (transform) are *catalog-only*: they are known GTS
//! identifiers but have no backing implementation and fail to resolve.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use credstore_sdk::{CredStoreClientV1, SecretRef};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::SecretString;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;

use crate::domain::ids;
use crate::domain::plugin::{
    AuthContext, AuthPlugin, GuardContext, GuardPlugin, PluginError, TransformContext,
    TransformPlugin,
};

/// OAuth2 token cache configuration (ADR-0008 §"Gear-Level Configuration").
#[derive(Debug, Clone)]
pub struct TokenCacheConfig {
    pub ttl: Duration,
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            capacity: 10_000,
        }
    }
}

/// Resolve a secret reference through the credential store. The configured
/// value may carry a `cred://` scheme prefix (stripped before lookup).
pub async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    security_context: &SecurityContext,
    secret_ref: &str,
) -> Result<SecretString, PluginError> {
    let key = secret_ref
        .strip_prefix("cred://")
        .unwrap_or(secret_ref)
        .trim();
    let reference = SecretRef::new(key.to_owned())
        .map_err(|e| PluginError::Internal(format!("invalid secret_ref '{secret_ref}': {e}")))?;
    let response = credstore
        .get(security_context, &reference)
        .await
        .map_err(|e| PluginError::Internal(format!("cred_store lookup failed: {e}")))?;
    let secret = response.ok_or_else(|| {
        PluginError::Internal(format!(
            "credential store returned no secret for '{secret_ref}'"
        ))
    })?;
    let value = secret.value;
    Ok(SecretString::new(
        String::from_utf8_lossy(value.as_bytes()).into_owned(),
    ))
}

/// Look up a string value from plugin config under any of the candidate
/// keys (case-insensitive), trimming whitespace.
fn config_string(config: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let obj = config.as_object()?;
    for key in keys {
        if let Some(value) = obj.get(*key) {
            if let Some(s) = value.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    None
}

fn config_bool(config: &serde_json::Value, keys: &[&str], default: bool) -> bool {
    let obj = config.as_object();
    if let Some(obj) = obj {
        for key in keys {
            if let Some(value) = obj.get(*key) {
                if let Some(b) = value.as_bool() {
                    return b;
                }
            }
        }
    }
    default
}

/// No-op authentication plugin — injects nothing.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        ids::auth::NOOP
    }

    async fn authenticate(&self, _ctx: &mut AuthContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// API-key authentication plugin: resolves the key via `cred_store` and
/// injects it in a header or query parameter.
///
/// Config keys: `header` (header name, default `X-API-Key`), `query`
/// (query parameter name — presence implies query mode), `name` / `in`
/// (legacy aliases), `secret_ref` / `key` / `api_key_ref` (the `cred://`
/// reference).
pub struct ApiKeyAuthPlugin;

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        ids::auth::APIKEY
    }

    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let secret_ref = config_string(
            &ctx.config,
            &["secret_ref", "api_key_ref", "key", "key_ref"],
        )
        .ok_or_else(|| PluginError::Internal("apikey plugin requires 'secret_ref'".to_owned()))?;

        let in_header = config_bool(&ctx.config, &["in_header"], true)
            && config_string(&ctx.config, &["query", "query_param"]).is_none();

        let location = config_string(&ctx.config, &["in"]).unwrap_or_else(|| {
            if in_header {
                "header".to_owned()
            } else {
                "query".to_owned()
            }
        });

        let secret =
            resolve_secret(ctx.credstore.as_ref(), &ctx.security_context, &secret_ref).await?;
        let value = secret.expose().to_owned();

        if location.eq_ignore_ascii_case("query") || !in_header && in_query(&ctx.config) {
            let name = config_string(&ctx.config, &["query", "query_param"])
                .unwrap_or_else(|| "api_key".to_owned());
            ctx.query_params.push((name, value));
        } else {
            let name = config_string(&ctx.config, &["header", "name"])
                .unwrap_or_else(|| "X-API-Key".to_owned());
            set_header(&mut ctx.headers, &name, &value);
        }
        Ok(())
    }
}

fn in_query(config: &serde_json::Value) -> bool {
    config_string(config, &["query", "query_param"]).is_some()
}

/// OAuth2 Client Credentials auth plugin with an internal token cache
/// (ADR-0008). Both `Form` and `Basic` client-auth variants share this
/// implementation; only `auth_method` differs.
pub struct OAuth2ClientCredAuthPlugin {
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

/// Cached token wrapper carrying the original cache key for collision
/// verification (ADR-0008 "Hash-Collision Safety").
#[derive(Clone)]
struct CachedToken {
    #[allow(dead_code)]
    key: String,
    token: SecretString,
}

impl OAuth2ClientCredAuthPlugin {
    pub fn new(auth_method: ClientAuthMethod, cache_ttl: Duration, cache_capacity: usize) -> Self {
        Self {
            auth_method,
            cache: MemoryCache::new(cache_capacity.max(1)),
            cache_ttl,
        }
    }

    /// Deterministic hash of the plugin config (sorted key/value pairs) so
    /// different upstream configs get distinct cache entries.
    fn hash_config(config: &serde_json::Value) -> String {
        use std::collections::BTreeMap;
        let obj = config.as_object().cloned().unwrap_or_default();
        let mut flat: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in obj {
            flat.insert(k.clone(), v.to_string());
        }
        // Simple stable digest: formatting the sorted map is deterministic.
        format!("{:?}", flat)
    }

    fn build_cache_key(ctx: &AuthContext, auth_method: ClientAuthMethod) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            auth_method_tag(auth_method),
            Self::hash_config(&ctx.config),
        )
    }
}

fn auth_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => "form",
        ClientAuthMethod::Basic => "basic",
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => ids::auth::OAUTH2_CLIENT_CRED,
            ClientAuthMethod::Basic => ids::auth::OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let key = Self::build_cache_key(ctx, self.auth_method);

        // Cache hit (with key verification for tiny-ufo hash-collision
        // safety — a mismatch is treated as a miss).
        if let (Some(entry), _status) = self.cache.get(&key) {
            if entry.key == key {
                inject_bearer(&mut ctx.headers, entry.token.expose());
                return Ok(());
            }
        }

        let client_id_ref = config_string(&ctx.config, &["client_id_ref", "client_id"])
            .ok_or_else(|| {
                PluginError::Internal("oauth2 plugin requires 'client_id_ref'".to_owned())
            })?;
        let client_secret_ref = config_string(&ctx.config, &["client_secret_ref", "client_secret"])
            .ok_or_else(|| {
                PluginError::Internal("oauth2 plugin requires 'client_secret_ref'".to_owned())
            })?;

        let client_id = resolve_secret(
            ctx.credstore.as_ref(),
            &ctx.security_context,
            &client_id_ref,
        )
        .await?
        .expose()
        .to_owned();
        let client_secret = resolve_secret(
            ctx.credstore.as_ref(),
            &ctx.security_context,
            &client_secret_ref,
        )
        .await?;

        let scopes: Vec<String> = config_string(&ctx.config, &["scopes"])
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();

        let token_endpoint = parse_url_opt(config_string(&ctx.config, &["token_endpoint"]))
            .map_err(|e| PluginError::Internal(format!("invalid token_endpoint: {e}")))?;
        let issuer_url = parse_url_opt(config_string(&ctx.config, &["issuer_url"]))
            .map_err(|e| PluginError::Internal(format!("invalid issuer_url: {e}")))?;
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(PluginError::Internal(
                "'token_endpoint' and 'issuer_url' are mutually exclusive".to_owned(),
            ));
        }

        let oauth_config = OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret,
            scopes,
            auth_method: self.auth_method,
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(1800),
            jitter_max: Duration::from_secs(300),
            min_refresh_period: Duration::from_secs(10),
            default_ttl: Duration::from_secs(300),
            http_config: None,
        };

        let fetched = fetch_token(oauth_config)
            .await
            .map_err(|e| PluginError::Internal(format!("token fetch failed: {e}")))?;

        // TTL = min(config_ttl, expires_in − 30s safety margin) —
        // tokens with expires_in ≤ 30s are not cached (ADR-0008).
        let ttl = fetched
            .expires_in
            .checked_sub(Duration::from_secs(30))
            .map(|margin| margin.min(self.cache_ttl))
            .unwrap_or(Duration::ZERO);

        if ttl > Duration::ZERO {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer.clone(),
                },
                Some(ttl),
            );
        }

        inject_bearer(&mut ctx.headers, fetched.bearer.expose());
        Ok(())
    }
}

fn parse_url_opt(value: Option<String>) -> Result<Option<Url>, url::ParseError> {
    value.map(|v| Url::parse(&v)).transpose()
}

fn inject_bearer(headers: &mut HeaderMap, token: &str) {
    insert_header_value(
        headers,
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}"),
    );
}

/// Required-headers guard plugin (ADR-0009).
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    async fn guard_request(&self, ctx: &GuardContext) -> Result<(), PluginError> {
        let Some(required) = config_string(&ctx.config, &["required_request_headers"]) else {
            return Ok(()); // fail-open when unconfigured
        };
        check_presence(&required, &ctx.headers)
            .map_err(PluginError::required_header_missing_request)
    }

    async fn guard_response(&self, ctx: &GuardContext) -> Result<(), PluginError> {
        let Some(required) = config_string(&ctx.config, &["required_response_headers"]) else {
            return Ok(()); // fail-open when unconfigured
        };
        check_presence(&required, &ctx.headers)
            .map_err(PluginError::required_header_missing_response)
    }
}

/// Parse the comma-separated config value and check every header (by
/// lowercase name) is present. Only the first missing header is reported
/// (ADR-0009).
fn check_presence(required_csv: &str, headers: &HeaderMap) -> Result<(), String> {
    for name in required_csv
        .split(',')
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| !s.is_empty())
    {
        let present = headers
            .keys()
            .any(|k| k.as_str().eq_ignore_ascii_case(&name));
        if !present {
            return Err(format!("required header '{name}' is missing"));
        }
    }
    Ok(())
}

/// Request-ID transform plugin: injects `X-Request-ID` on the request when
/// absent and propagates it to the response (DESIGN §3.2).
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    async fn transform_request(&self, ctx: &mut TransformContext) -> Result<(), PluginError> {
        let header = config_string(&ctx.config, &["header_name"])
            .unwrap_or_else(|| "X-Request-ID".to_owned());
        if !ctx.headers.contains_key(&header) {
            set_header(&mut ctx.headers, &header, &uuid::Uuid::new_v4().to_string());
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut TransformContext) -> Result<(), PluginError> {
        let header = config_string(&ctx.config, &["header_name"])
            .unwrap_or_else(|| "X-Request-ID".to_owned());
        let request_value = ctx
            .headers
            .get(&header)
            .cloned()
            .or_else(|| ctx.headers.get("x-request-id").cloned());
        if let Some(value) = request_value {
            if HeaderName::from_bytes(header.as_bytes()).is_ok() {
                ctx.headers.insert(
                    HeaderName::from_bytes(header.as_bytes()).expect("name parsed"),
                    value,
                );
            }
        }
        Ok(())
    }
}

fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
        return;
    };
    headers.insert(name, value);
}

fn insert_header_value(headers: &mut HeaderMap, name: HeaderName, value: String) {
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

/// Registry of resolvable built-in plugins.
pub struct PluginRegistry {
    pub auth: HashMap<String, Arc<dyn AuthPlugin>>,
    pub guard: HashMap<String, Arc<dyn GuardPlugin>>,
    pub transform: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl PluginRegistry {
    /// Register every built-in plugin.
    #[must_use]
    pub fn with_builtins(token_cache: &TokenCacheConfig) -> Self {
        let mut auth: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        auth.insert(ids::auth::NOOP.to_owned(), Arc::new(NoopAuthPlugin));
        auth.insert(ids::auth::APIKEY.to_owned(), Arc::new(ApiKeyAuthPlugin));
        auth.insert(
            ids::auth::OAUTH2_CLIENT_CRED.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                ClientAuthMethod::Form,
                token_cache.ttl,
                token_cache.capacity,
            )),
        );
        auth.insert(
            ids::auth::OAUTH2_CLIENT_CRED_BASIC.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                ClientAuthMethod::Basic,
                token_cache.ttl,
                token_cache.capacity,
            )),
        );

        let mut guard: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        guard.insert(
            ids::guard::REQUIRED_HEADERS.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );

        let mut transform: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        transform.insert(
            ids::transform::REQUEST_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );

        Self {
            auth,
            guard,
            transform,
        }
    }

    /// Resolve an auth plugin by full GTS identifier, or `None`.
    #[must_use]
    pub fn resolve_auth(&self, gts_id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.auth.get(gts_id).cloned()
    }

    /// Resolve a guard plugin by full GTS identifier, or `None`.
    #[must_use]
    pub fn resolve_guard(&self, gts_id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.guard.get(gts_id).cloned()
    }

    /// Resolve a transform plugin by full GTS identifier, or `None`.
    #[must_use]
    pub fn resolve_transform(&self, gts_id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.transform.get(gts_id).cloned()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    /// Run an async plugin method from a sync test.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("build tokio runtime")
            .block_on(fut)
    }

    use super::*;

    // Minimal in-process CredStore double returning configured values.
    #[derive(Default)]
    struct TestCredStore {
        values: parking_lot::Mutex<HashMap<String, String>>,
    }

    impl TestCredStore {
        fn seed(&self, key: &str, value: &str) {
            self.values.lock().insert(key.to_owned(), value.to_owned());
        }
    }

    #[async_trait]
    impl CredStoreClientV1 for TestCredStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            key: &SecretRef,
        ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError>
        {
            use credstore_sdk::SharingMode as Shared;
            let values = self.values.lock();
            let Some(value) = values.get(key.as_ref()) else {
                return Ok(None);
            };
            Ok(Some(credstore_sdk::GetSecretResponse {
                value: credstore_sdk::SecretValue::new(value.clone().into_bytes()),
                id: uuid::Uuid::nil(),
                owner_tenant_id: credstore_sdk::TenantId::nil(),
                sharing: Shared::Private,
                is_inherited: false,
                version: 1,
                secret_type: "gts.cf.core.credstore.secret_type.v1~cf.core.credstore.opaque.v1"
                    .to_owned(),
                expires_at: None,
            }))
        }
    }

    #[test]
    fn registry_resolves_all_builtins() {
        let registry = PluginRegistry::with_builtins(&TokenCacheConfig::default());
        assert!(registry.resolve_auth(ids::auth::NOOP).is_some());
        assert!(registry.resolve_auth(ids::auth::APIKEY).is_some());
        assert!(
            registry
                .resolve_auth(ids::auth::OAUTH2_CLIENT_CRED)
                .is_some()
        );
        assert!(
            registry
                .resolve_auth(ids::auth::OAUTH2_CLIENT_CRED_BASIC)
                .is_some()
        );
        assert!(
            registry
                .resolve_guard(ids::guard::REQUIRED_HEADERS)
                .is_some()
        );
        assert!(
            registry
                .resolve_transform(ids::transform::REQUEST_ID)
                .is_some()
        );
    }

    #[test]
    fn catalog_only_ids_do_not_resolve() {
        let registry = PluginRegistry::with_builtins(&TokenCacheConfig::default());
        assert!(registry.resolve_auth(ids::auth::BASIC).is_none());
        assert!(registry.resolve_auth(ids::auth::BEARER).is_none());
        assert!(registry.resolve_guard(ids::guard::TIMEOUT).is_none());
        assert!(registry.resolve_guard(ids::guard::CORS).is_none());
        assert!(
            registry
                .resolve_transform(ids::transform::LOGGING)
                .is_none()
        );
        assert!(
            registry
                .resolve_transform(ids::transform::METRICS)
                .is_none()
        );
    }

    #[test]
    fn required_headers_fail_open_when_unconfigured() {
        let config = serde_json::json!({});
        let headers = HeaderMap::new();
        let ctx = GuardContext {
            headers,
            is_response: false,
            config: config.clone(),
        };
        let plugin = RequiredHeadersGuardPlugin;
        let result = block_on(plugin.guard_request(&ctx));
        assert!(result.is_ok());
    }

    #[test]
    fn required_headers_reject_missing() {
        let config = serde_json::json!({
            "required_request_headers": "x-correlation-id, accept"
        });
        let headers = HeaderMap::new();
        let ctx = GuardContext {
            headers,
            is_response: false,
            config,
        };
        let plugin = RequiredHeadersGuardPlugin;
        let result = block_on(plugin.guard_request(&ctx));
        assert!(result.is_err());
        let error = result.unwrap_err();
        match error {
            PluginError::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn required_headers_pass_when_present() {
        let config = serde_json::json!({ "required_request_headers": "x-correlation-id,accept" });
        let mut headers = HeaderMap::new();
        headers.insert("x-correlation-id", HeaderValue::from_static("abc"));
        headers.insert("accept", HeaderValue::from_static("application/json"));
        let ctx = GuardContext {
            headers,
            is_response: false,
            config,
        };
        let plugin = RequiredHeadersGuardPlugin;
        let result = block_on(plugin.guard_request(&ctx));
        assert!(result.is_ok());
    }

    #[test]
    fn request_id_injects_and_propagates() {
        let config = serde_json::json!({});
        let mut ctx = TransformContext {
            config: config.clone(),
            headers: HeaderMap::new(),
        };
        let plugin = RequestIdTransformPlugin;
        block_on(plugin.transform_request(&mut ctx)).unwrap();
        let request_id = ctx.headers.get("x-request-id").cloned().unwrap();
        assert!(!request_id.to_str().unwrap().is_empty());

        let mut response_ctx = TransformContext {
            config,
            headers: HeaderMap::new(),
        };
        response_ctx
            .headers
            .insert("x-request-id", request_id.clone());
        block_on(plugin.transform_response(&mut response_ctx)).unwrap();
        assert_eq!(response_ctx.headers.get("x-request-id"), Some(&request_id));
    }

    #[tokio::test]
    async fn apikey_injects_header_from_credstore() {
        let store = Arc::new(TestCredStore::default());
        store.seed("my-key", "super-secret");
        let mut ctx = AuthContext {
            config: serde_json::json!({
                "header": "X-API-Key",
                "secret_ref": "cred://my-key"
            }),
            headers: HeaderMap::new(),
            query_params: Vec::new(),
            security_context: SecurityContext::anonymous(),
            credstore: store,
        };
        let plugin = ApiKeyAuthPlugin;
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get("x-api-key").unwrap().to_str().unwrap(),
            "super-secret"
        );
    }

    #[tokio::test]
    async fn apikey_injects_query_when_configured() {
        let store = Arc::new(TestCredStore::default());
        store.seed("my-key", "q-value");
        let mut ctx = AuthContext {
            config: serde_json::json!({
                "query": "api_key",
                "secret_ref": "cred://my-key"
            }),
            headers: HeaderMap::new(),
            query_params: Vec::new(),
            security_context: SecurityContext::anonymous(),
            credstore: store,
        };
        let plugin = ApiKeyAuthPlugin;
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.query_params,
            vec![("api_key".to_owned(), "q-value".to_owned())]
        );
    }

    #[tokio::test]
    async fn apikey_missing_secret_maps_to_internal() {
        let store = Arc::new(TestCredStore::default());
        let mut ctx = AuthContext {
            config: serde_json::json!({ "header": "X-API-Key", "secret_ref": "cred://gone" }),
            headers: HeaderMap::new(),
            query_params: Vec::new(),
            security_context: SecurityContext::anonymous(),
            credstore: store,
        };
        let plugin = ApiKeyAuthPlugin;
        let result = plugin.authenticate(&mut ctx).await;
        assert!(matches!(result, Err(PluginError::Internal(_))));
    }
}
