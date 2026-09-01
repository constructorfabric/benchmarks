//! Built-in plugins (ADR-0002 / ADR-0008 / ADR-0009).
//!
//! Registered in the in-process plugin registries:
//! * Auth: `noop`, `apikey`, `oauth2_client_cred` (Form), `oauth2_client_cred_basic`
//! * Guard: `required_headers`
//! * Transform: `request_id`
//!
//! `basic`/`bearer`, `timeout`/`cors`, `logging`/`metrics` are catalog-only
//! identifiers — not resolvable here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;

use credstore_sdk::CredStoreClientV1;
use credstore_sdk::models::SecretRef;

use crate::domain::dto::{
    AUTH_APIKEY, AUTH_OAUTH2_BASIC, AUTH_OAUTH2_FORM, GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID,
};
use crate::domain::merge::hash_config;
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginError, RequestContext,
    ResponseContext, TransformPlugin,
};

/// Extract a `cred://` reference from a plugin config string.
fn strip_cred_prefix(value: &str) -> &str {
    value.strip_prefix("cred://").unwrap_or(value)
}

fn parse_u64_field(config: &Value, key: &str) -> Option<u64> {
    config.get(key).and_then(serde_json::Value::as_u64)
}

fn parse_str_field<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// NoopAuthPlugin
// ---------------------------------------------------------------------------

/// No authentication; passes headers through untouched.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::dto::AUTH_NOOP
    }

    fn plugin_type(&self) -> &'static str {
        "noop"
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ApiKeyAuthPlugin
// ---------------------------------------------------------------------------

/// Injects an API key into a header (or query parameter) from `cred_store`.
///
/// Config keys:
/// * `secret_ref` (required) — `cred://` reference to the API key value
/// * `header` (optional, default `X-API-Key`) — header to set
/// * `query` (optional) — query parameter to set instead of a header
pub struct ApiKeyAuthPlugin {
    credstore: Option<Arc<dyn CredStoreClientV1>>,
}

impl ApiKeyAuthPlugin {
    #[must_use]
    pub fn new(credstore: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { credstore }
    }
}

fn urlencode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        AUTH_APIKEY
    }

    fn plugin_type(&self) -> &'static str {
        "apikey"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = &ctx.config;
        let Some(secret_ref) = parse_str_field(config, "secret_ref") else {
            return Err(PluginError::Config(
                "apikey plugin requires a 'secret_ref' config key".into(),
            ));
        };
        let Some(credstore) = &self.credstore else {
            return Err(PluginError::Config(
                "apikey plugin is configured but no credentials store is available".into(),
            ));
        };
        let ref_ = SecretRef::new(strip_cred_prefix(secret_ref))
            .map_err(|e| PluginError::AuthFailed(format!("invalid secret ref: {e}")))?;
        let secret = credstore
            .get(&ctx.security_context, &ref_)
            .await
            .map_err(|e| PluginError::AuthFailed(format!("credential store error: {e}")))?;
        let Some(secret) = secret else {
            return Err(PluginError::AuthFailed(format!(
                "secret not found: {secret_ref}"
            )));
        };
        let value = String::from_utf8_lossy(secret.value.as_bytes()).into_owned();

        let header_name = parse_str_field(config, "header").unwrap_or("X-API-Key");
        if let Some(query) = parse_str_field(config, "query") {
            // Query injection is represented via a synthetic URI.
            let query_to_append = format!("{query}={}", urlencode(&value));
            let sep = if ctx.uri.query().is_some_and(|q| !q.is_empty()) {
                "&"
            } else {
                "?"
            };
            let new_query = format!(
                "{}{}{}",
                ctx.uri.query().unwrap_or(""),
                sep,
                query_to_append
            );
            let mut builder = http::Uri::builder().scheme(
                ctx.uri
                    .scheme()
                    .cloned()
                    .unwrap_or(http::uri::Scheme::HTTPS),
            );
            if let Some(auth) = ctx.uri.authority() {
                builder = builder.authority(auth.clone());
            }
            ctx.uri = builder
                .path_and_query(format!("{}{}", ctx.uri.path(), new_query).as_str())
                .build()
                .map_err(|e| PluginError::Internal(format!("uri build failed: {e}")))?;
        } else {
            ctx.headers.insert(
                http::header::HeaderName::from_bytes(header_name.as_bytes())
                    .map_err(|e| PluginError::Config(format!("invalid header name: {e}")))?,
                http::HeaderValue::from_str(&value)
                    .map_err(|e| PluginError::AuthFailed(format!("invalid key value: {e}")))?,
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OAuth2 client credentials (Form) / (Basic)  — ADR-0008
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct OAuth2ConfigShape {
    token_endpoint: Option<Url>,
    issuer_url: Option<Url>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Vec<String>,
    cache_ttl_secs: u64,
}

impl OAuth2ConfigShape {
    fn parse(config: &Value, default_ttl_secs: u64) -> Result<Self, PluginError> {
        let token_endpoint = parse_str_field(config, "token_endpoint")
            .map(|s| {
                Url::parse(s).map_err(|e| PluginError::Config(format!("bad token_endpoint: {e}")))
            })
            .transpose()?;
        let issuer_url = parse_str_field(config, "issuer_url")
            .map(|s| Url::parse(s).map_err(|e| PluginError::Config(format!("bad issuer_url: {e}"))))
            .transpose()?;
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(PluginError::Config(
                "token_endpoint and issuer_url are mutually exclusive".into(),
            ));
        }
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(PluginError::Config(
                "one of token_endpoint or issuer_url must be set".into(),
            ));
        }
        let client_id_ref = parse_str_field(config, "client_id_ref")
            .ok_or_else(|| PluginError::Config("client_id_ref is required".into()))?
            .to_owned();
        let client_secret_ref = parse_str_field(config, "client_secret_ref")
            .ok_or_else(|| PluginError::Config("client_secret_ref is required".into()))?
            .to_owned();
        let scopes = parse_str_field(config, "scopes")
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        let cache_ttl_secs =
            parse_u64_field(config, "token_cache_ttl_secs").unwrap_or(default_ttl_secs);
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes,
            cache_ttl_secs,
        })
    }
}

#[derive(Clone)]
struct CachedToken {
    /// The cache key this entry was stored under.  Verified on every hit so a
    /// recycled memory-cache slot can never serve a token fetched for a
    /// different (tenant, subject, config) key (ADR-0008).
    key: String,
    // Plain string: SecretString is deliberately not Clone; the value is only
    // copied on the way into the header, never logged.
    bearer: String,
    expires_unix_secs: i64,
}

/// `OAuth2` client credentials auth plugin.
///
/// Shares the implementation between the Form and Basic variants with an
/// internal `pingora-memory-cache` token cache (ADR-0008).  Cache key covers
/// (tenant, subject, config, auth-method) so distinct upstream configs never
/// collide.
pub struct OAuth2ClientCredAuthPlugin {
    auth_method: ClientAuthMethod,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl_secs: u64,
}

impl OAuth2ClientCredAuthPlugin {
    #[must_use]
    pub fn new(
        auth_method: ClientAuthMethod,
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        cache_capacity: usize,
        cache_ttl_secs: u64,
    ) -> Self {
        Self {
            auth_method,
            credstore,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl_secs,
        }
    }

    fn gts_id(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => AUTH_OAUTH2_FORM,
            ClientAuthMethod::Basic => AUTH_OAUTH2_BASIC,
        }
    }

    fn plugin_type_name(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Form => "oauth2_client_cred",
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
        }
    }

    fn cache_key(&self, ctx: &RequestContext, config_hash: u64) -> String {
        format!(
            "{}-{}-{}-{}-{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.plugin_type_name(),
            config_hash,
            self.cache_ttl_secs,
        )
    }
}

const TOKEN_SAFETY_MARGIN_SECS: i64 = 30;

/// Cache TTL for a fetched `OAuth2` token, or `None` when the token must NOT be
/// cached because its remaining lifetime is at/below the 30s safety margin
/// (ADR-0008 — such a token is stale on the next lookup and would only waste
/// cache room).  Otherwise `min(config_ttl, expires_in − 30s)`.
fn cache_ttl_for(expires_in: i64, config_ttl_secs: u64) -> Option<Duration> {
    if expires_in <= TOKEN_SAFETY_MARGIN_SECS {
        return None;
    }
    // Checked conversions: config_ttl_secs is a non-negative duration that can
    // never wrap i64 (saturating to i64::MAX); the final value is >= 1 by the
    // `.max(1)` below, so the u64 re-conversion cannot lose the sign.
    let ttl = i64::try_from(config_ttl_secs)
        .unwrap_or(i64::MAX)
        .min(expires_in - TOKEN_SAFETY_MARGIN_SECS)
        .max(1);
    Some(Duration::from_secs(u64::try_from(ttl).unwrap_or(u64::MAX)))
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        self.gts_id()
    }

    fn plugin_type(&self) -> &str {
        self.plugin_type_name()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let shape = OAuth2ConfigShape::parse(&ctx.config, self.cache_ttl_secs)?;
        let config_hash = hash_config(&ctx.config);
        let key = self.cache_key(ctx, config_hash);

        // Epoch seconds fit comfortably in i64 for centuries to come; clamp
        // rather than truncate on the (impossible) overflow.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

        // Cache hit — serve the token while it is still valid beyond the
        // safety margin, AND only when the entry was stored under this exact
        // key (a recycled memory-cache slot must not leak another config's
        // token — mismatch is treated as a miss).
        if let (Some(token), _) = self.cache.get(&key)
            && token.key == key
            && token.expires_unix_secs - TOKEN_SAFETY_MARGIN_SECS > now
        {
            ctx.headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", token.bearer))
                    .map_err(|e| PluginError::Internal(format!("bad token in header: {e}")))?,
            );
            return Ok(());
        }

        let Some(credstore) = &self.credstore else {
            return Err(PluginError::Config(
                "oauth2 plugin is configured but no credentials store is available".into(),
            ));
        };

        let client_id = self
            .resolve_secret(credstore, &ctx.security_context, &shape.client_id_ref)
            .await?;
        let client_secret = self
            .resolve_secret(credstore, &ctx.security_context, &shape.client_secret_ref)
            .await?;

        let token_cfg = OAuthClientConfig {
            token_endpoint: shape.token_endpoint.clone(),
            issuer_url: shape.issuer_url.clone(),
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes: shape.scopes.clone(),
            auth_method: self.auth_method,
            ..Default::default()
        };
        let fetched = fetch_token(token_cfg)
            .await
            .map_err(|e| PluginError::AuthFailed(format!("token fetch failed: {e}")))?;

        // `expires_in` is a wall-clock duration (never negative); it only
        // overflows i64 after ~292 billion years, saturate then.
        let expires_in = i64::try_from(fetched.expires_in.as_secs()).unwrap_or(i64::MAX);
        let bearer = fetched.bearer.expose().to_owned();
        let cached = CachedToken {
            key: key.clone(),
            bearer: bearer.clone(),
            expires_unix_secs: now + expires_in,
        };
        // ADR-0008: a token whose remaining lifetime is at/below the 30s
        // safety margin is already stale for the next lookup (the hit guard
        // would refetch it anyway) — do NOT cache it.
        if let Some(ttl) = cache_ttl_for(expires_in, shape.cache_ttl_secs) {
            self.cache.put(&key, cached, Some(ttl));
        }

        ctx.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {bearer}"))
                .map_err(|e| PluginError::Internal(format!("bad token in header: {e}")))?,
        );
        Ok(())
    }
}

impl OAuth2ClientCredAuthPlugin {
    async fn resolve_secret(
        &self,
        credstore: &Arc<dyn CredStoreClientV1>,
        sctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<String, PluginError> {
        let ref_ = SecretRef::new(strip_cred_prefix(secret_ref))
            .map_err(|e| PluginError::AuthFailed(format!("invalid secret ref: {e}")))?;
        let secret = credstore
            .get(sctx, &ref_)
            .await
            .map_err(|e| PluginError::AuthFailed(format!("credential store error: {e}")))?;
        let Some(secret) = secret else {
            return Err(PluginError::AuthFailed(format!(
                "secret not found: {secret_ref}"
            )));
        };
        Ok(String::from_utf8_lossy(secret.value.as_bytes()).into_owned())
    }
}

// ---------------------------------------------------------------------------
// RequiredHeadersGuardPlugin — ADR-0009
// ---------------------------------------------------------------------------

/// Guards request/response header presence.
///
/// Config keys (both optional, fail-open when absent/blank):
/// * `required_request_headers` — comma-separated names checked in `guard_request`
/// * `required_response_headers` — comma-separated names checked in `guard_response`
pub struct RequiredHeadersGuardPlugin;

fn parse_header_list(config: &Value, key: &str) -> Vec<String> {
    parse_str_field(config, key)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

fn header_present(headers: &http::HeaderMap, name: &str) -> bool {
    headers
        .keys()
        .any(|k| k.as_str().eq_ignore_ascii_case(name))
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &'static str {
        "required_headers"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(&ctx.config, "required_request_headers");
        for name in required {
            if !header_present(&ctx.headers, &name) {
                return Ok(GuardDecision::Reject {
                    status: 400,
                    error_code: "REQUIRED_HEADER_MISSING".into(),
                    detail: format!("required request header is missing: {name}"),
                });
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(&ctx.config, "required_response_headers");
        for name in required {
            if !header_present(&ctx.headers, &name) {
                return Ok(GuardDecision::Reject {
                    status: 502,
                    error_code: "REQUIRED_HEADER_MISSING".into(),
                    detail: format!("required response header is missing: {name}"),
                });
            }
        }
        Ok(GuardDecision::Allow)
    }
}

// ---------------------------------------------------------------------------
// RequestIdTransformPlugin
// ---------------------------------------------------------------------------

/// Injects/propagates `X-Request-ID` across the proxy hop and echoes it on
/// the response.  When the client already sent an `X-Request-ID` it is
/// propagated; otherwise a fresh ID is generated.
pub struct RequestIdTransformPlugin;

const REQUEST_ID_HEADER: &str = "x-request-id";

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &'static str {
        "request_id"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let existing = ctx
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let request_id = match existing {
            Some(id) if !id.is_empty() => id,
            _ => format!("req_{}", uuid::Uuid::new_v4().simple()),
        };
        ctx.headers.insert(
            http::header::HeaderName::from_static(REQUEST_ID_HEADER),
            http::HeaderValue::from_str(&request_id)
                .map_err(|e| PluginError::Internal(format!("bad request id: {e}")))?,
        );
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        // Echo the ID that was sent upstream (captured by the data plane).
        if let Some(request_id) = &ctx.request_id {
            ctx.headers.insert(
                http::header::HeaderName::from_static(REQUEST_ID_HEADER),
                http::HeaderValue::from_str(request_id)
                    .map_err(|e| PluginError::Internal(format!("bad request id: {e}")))?,
            );
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        // Errors produced before request transform complete carry no
        // populated request ID; nothing to add.
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use http::HeaderMap;
    use serde_json::json;
    use std::sync::Arc;

    fn request_ctx(config: Value) -> RequestContext {
        RequestContext {
            config,
            method: http::Method::GET,
            uri: "http://oagw.example.com/proxy/x".parse().unwrap(),
            headers: HeaderMap::new(),
            security_context: SecurityContext::anonymous(),
        }
    }

    fn mock_credstore(pairs: Vec<(String, String)>) -> Arc<dyn CredStoreClientV1> {
        Arc::new(MockCredStoreClient::with_secrets(pairs))
    }

    #[tokio::test]
    async fn noop_does_nothing() {
        let mut ctx = request_ctx(Value::Null);
        NoopAuthPlugin.authenticate(&mut ctx).await.unwrap();
        assert!(ctx.headers.is_empty());
    }

    #[tokio::test]
    async fn noop_is_standard_gts_id() {
        assert_eq!(NoopAuthPlugin.id(), crate::domain::dto::AUTH_NOOP);
    }

    #[tokio::test]
    async fn apikey_missing_secret_ref_is_config_error() {
        let mut ctx = request_ctx(json!({}));
        let plugin = ApiKeyAuthPlugin::new(None);
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn apikey_missing_secret_is_auth_failure() {
        let mut ctx = request_ctx(json!({"secret_ref": "cred://none"}));
        let plugin = ApiKeyAuthPlugin::new(Some(mock_credstore(vec![])));
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn apikey_injects_header_from_credstore() {
        let mut ctx = request_ctx(json!({"secret_ref": "cred://my-key"}));
        let plugin = ApiKeyAuthPlugin::new(Some(mock_credstore(vec![(
            "my-key".into(),
            "sk-secret".into(),
        )])));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "sk-secret");
    }

    #[tokio::test]
    async fn apikey_honors_custom_header_name() {
        let mut ctx = request_ctx(json!({"secret_ref": "cred://k", "header": "X-Custom-Key"}));
        let plugin = ApiKeyAuthPlugin::new(Some(mock_credstore(vec![("k".into(), "v".into())])));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.headers.get("x-custom-key").unwrap(), "v");
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_missing_request_header() {
        let ctx = request_ctx(json!({"required_request_headers": "x-correlation-id, accept"}));
        let decision = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .unwrap();
        match decision {
            GuardDecision::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn required_headers_guard_allows_when_present() {
        let mut ctx = request_ctx(json!({"required_request_headers": "x-correlation-id"}));
        ctx.headers
            .insert("x-correlation-id", http::HeaderValue::from_static("abc"));
        assert_eq!(
            RequiredHeadersGuardPlugin
                .guard_request(&ctx)
                .await
                .unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn required_headers_guard_fail_open_unconfigured() {
        let ctx = request_ctx(Value::Null);
        assert_eq!(
            RequiredHeadersGuardPlugin
                .guard_request(&ctx)
                .await
                .unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_missing_response_header() {
        let ctx = ResponseContext {
            config: json!({"required_response_headers": "content-type"}),
            status: http::StatusCode::OK,
            headers: HeaderMap::new(),
            request_id: None,
        };
        match RequiredHeadersGuardPlugin
            .guard_response(&ctx)
            .await
            .unwrap()
        {
            GuardDecision::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, 502);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn request_id_propagates_and_echoes() {
        let mut ctx = request_ctx(Value::Null);
        ctx.headers.insert(
            "x-request-id",
            http::HeaderValue::from_static("client-id-42"),
        );
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.headers.get("x-request-id").unwrap(), "client-id-42");

        let mut resp = ResponseContext {
            config: Value::Null,
            status: http::StatusCode::OK,
            headers: HeaderMap::new(),
            request_id: Some("client-id-42".into()),
        };
        RequestIdTransformPlugin
            .transform_response(&mut resp)
            .await
            .unwrap();
        assert_eq!(resp.headers.get("x-request-id").unwrap(), "client-id-42");
    }

    #[tokio::test]
    async fn request_id_generates_fresh_when_absent() {
        let mut ctx = request_ctx(Value::Null);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        let id = ctx.headers.get("x-request-id").unwrap().to_str().unwrap();
        assert!(id.starts_with("req_"));
    }

    #[tokio::test]
    async fn oauth2_config_validation_rejects_both_endpoints() {
        let err = OAuth2ConfigShape::parse(
            &json!({
                "token_endpoint": "https://a.example/token",
                "issuer_url": "https://b.example",
                "client_id_ref": "cred://id",
                "client_secret_ref": "cred://secret",
            }),
            300,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn oauth2_config_validation_rejects_neither_endpoint() {
        let err = OAuth2ConfigShape::parse(
            &json!({
                "client_id_ref": "cred://id",
                "client_secret_ref": "cred://secret",
            }),
            300,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn oauth2_missing_secret_is_auth_failure() {
        let mut ctx = request_ctx(json!({
            "token_endpoint": "http://127.0.0.1:1/token",
            "client_id_ref": "cred://none",
            "client_secret_ref": "cred://none"
        }));
        let plugin = OAuth2ClientCredAuthPlugin::new(
            ClientAuthMethod::Form,
            Some(mock_credstore(vec![])),
            100,
            300,
        );
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn oauth2_plugin_ids_are_gts() {
        let form = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Form, None, 100, 300);
        assert_eq!(form.id(), AUTH_OAUTH2_FORM);
        let basic = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Basic, None, 100, 300);
        assert_eq!(basic.id(), AUTH_OAUTH2_BASIC);
    }

    #[test]
    fn oauth2_short_lived_tokens_are_not_cached() {
        // ADR-0008: a token at/below the 30s safety margin must never be
        // cached (it would be stale on the next lookup).
        assert!(cache_ttl_for(30, 300).is_none()); // == margin
        assert!(cache_ttl_for(10, 300).is_none()); // < margin
        assert!(cache_ttl_for(0, 300).is_none());
        // Long-lived token: ttl = min(config_ttl, expires_in − 30s), bounded
        // by the configured cache ttl.
        assert_eq!(cache_ttl_for(3600, 300), Some(Duration::from_mins(5)));
        // expires_in = 60 → ttl = min(300, 30) = 30.
        assert_eq!(cache_ttl_for(60, 300), Some(Duration::from_secs(30)));
        // expires_in = 31 → 1s floor.
        assert_eq!(cache_ttl_for(31, 300), Some(Duration::from_secs(1)));
    }

    fn oauth2_plugin_with_preseed(key: &str) -> OAuth2ClientCredAuthPlugin {
        let plugin = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Form, None, 8, 300);
        // Only reachable via a cache hit — the plugin has NO credstore and NO
        // network, so a miss necessarily fails with a Config error.
        let key = key.to_owned();
        plugin.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                bearer: "tok-cached".to_owned(),
                expires_unix_secs: 4_000_000_000 + 3600,
            },
            Some(Duration::from_mins(5)),
        );
        plugin
    }

    #[tokio::test]
    async fn oauth2_cached_token_served_when_key_matches() {
        let mut ctx = request_ctx(json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://cid",
            "client_secret_ref": "cred://csec",
        }));
        let key = {
            let hash = hash_config(&ctx.config);
            let plugin = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Form, None, 8, 300);
            plugin.cache_key(&ctx, hash)
        };
        let plugin = oauth2_plugin_with_preseed(&key);
        // Cache hit serves the token without fetching (no credstore configured).
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok-cached"
        );
    }

    #[tokio::test]
    async fn oauth2_cached_token_key_mismatch_is_treated_as_miss() {
        let mut ctx = request_ctx(json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://cid",
            "client_secret_ref": "cred://csec",
        }));
        let hash = hash_config(&ctx.config);
        let plugin = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Form, None, 8, 300);
        let key = plugin.cache_key(&ctx, hash);
        // Simulate a recycled memory-cache slot holding a token for another
        // key: the entry was stored under `key` but claims a different key.
        plugin.cache.put(
            &key,
            CachedToken {
                key: "some-other-tenant".to_owned(),
                bearer: "tok-stale".to_owned(),
                expires_unix_secs: 4_000_000_000 + 3600,
            },
            Some(Duration::from_mins(5)),
        );
        // Mismatch → miss → with no credstore, authenticate must fail rather
        // than serve the foreign token.
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn oauth2_expired_cached_token_not_served() {
        let mut ctx = request_ctx(json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://cid",
            "client_secret_ref": "cred://csec",
        }));
        let hash = hash_config(&ctx.config);
        let plugin = OAuth2ClientCredAuthPlugin::new(ClientAuthMethod::Form, None, 8, 300);
        let key = plugin.cache_key(&ctx, hash);
        // Correct key but stale (within the 30s safety margin) → miss.
        plugin.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                bearer: "tok-stale".to_owned(),
                expires_unix_secs: 1,
            },
            Some(Duration::from_mins(5)),
        );
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }
}
