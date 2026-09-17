//! Built-in plugin implementations (DESIGN §3.2, ADR 0002/0008/0009).
//!
//! - Auth: `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`.
//! - Guard: `required_headers`.
//! - Transform: `request_id`.
//!
//! Catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`) have **no** implementations here — the registries refuse to
//! resolve them and the data plane surfaces `503 plugin.not_found`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use http::header::CONTENT_TYPE;
use http::{HeaderValue, Method, Request};
use serde_json::Value;
use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::gts as g;
use crate::domain::plugin::{
    AuthPlugin, GuardPlugin, PluginError, ProxyRequestView, ProxyResponseView, SecretResolver,
    TransformPlugin, validation_rejected,
};
use crate::infra::proxy::client::OagwHttpClient;

// ---------------------------------------------------------------------------
// Auth: noop
// ---------------------------------------------------------------------------

/// Inject nothing. Used when no auth is configured.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        g::AUTH_NOOP
    }

    async fn inject_credentials(
        &self,
        _request: &mut ProxyRequestView,
        _config: &Value,
        _secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Auth: apikey (header/query injection)
// ---------------------------------------------------------------------------

/// API key injection (header/query). Config keys:
///
/// - `header` (string, default `x-api-key`) — header to populate.
/// - `query_param` (string, optional) — query parameter to populate.
/// - `key_ref` (string, `cred://` reference) — required unless `key` set.
/// - `key` (string, static value) — alternative to `key_ref`.
pub struct ApiKeyAuthPlugin;

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        g::AUTH_APIKEY
    }

    async fn inject_credentials(
        &self,
        request: &mut ProxyRequestView,
        config: &Value,
        secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        let header = config
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("x-api-key")
            .to_ascii_lowercase();
        let query_param = config.get("query_param").and_then(Value::as_str);

        let key: String = if let Some(reference) = config.get("key_ref").and_then(Value::as_str) {
            let bytes = secrets
                .resolve(request.tenant_id, reference)
                .await
                .map_err(|e| PluginError::External(format!("key resolution failed: {e}")))?;
            String::from_utf8(bytes)
                .map_err(|_| PluginError::External("api key is not valid UTF-8".to_owned()))?
        } else if let Some(key) = config.get("key").and_then(Value::as_str) {
            key.to_owned()
        } else {
            return Err(PluginError::InvalidConfig(
                "apikey auth requires 'key_ref' or 'key'".to_owned(),
            ));
        };

        if let Ok(name) = http::header::HeaderName::from_bytes(header.as_bytes()) {
            if let Ok(value) = HeaderValue::from_str(&key) {
                request.headers.insert(name, value);
            }
        }
        if let Some(param) = query_param {
            append_query_param(request, param, &key);
        }
        Ok(())
    }
}

fn append_query_param(request: &mut ProxyRequestView, name: &str, value: &str) {
    use std::fmt::Write as _;
    let encoded = urlencoding(name, value);
    let mut query = String::new();
    if !request.query.is_empty() {
        query.push_str(&request.query);
        query.push('&');
    }
    let _ = write!(query, "{encoded}");
    request.query = query;
}

fn urlencoding(key: &str, value: &str) -> String {
    let mut out = String::new();
    for byte in key.bytes().chain(std::iter::once(b'=')).chain(value.bytes()) {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'=' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Auth: oauth2 client credentials (Form / Basic) with token cache (ADR 0008)
// ---------------------------------------------------------------------------

/// Cached access token, verified against the lookup key on every hit to
/// defend against hash collisions (ADR 0008 §"Hash-Collision Safety").
struct CachedToken {
    key: String,
    token: String,
    cached_at: Instant,
    ttl: Duration,
}

impl CachedToken {
    fn valid(&self, key: &str, now: Instant) -> bool {
        self.key == key && now.duration_since(self.cached_at) < self.ttl
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    Form,
    Basic,
}

impl ClientAuthMethod {
    fn tag(&self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

struct OAuth2Config {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Option<String>,
}

impl OAuth2Config {
    fn parse(config: &Value) -> Result<Self, PluginError> {
        let token_endpoint = config.get("token_endpoint").and_then(Value::as_str);
        let issuer_url = config.get("issuer_url").and_then(Value::as_str);
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::InvalidConfig(
                "oauth2 client-credentials config requires exactly one of 'token_endpoint' or 'issuer_url'"
                    .to_owned(),
            ));
        }
        let client_id_ref = config
            .get("client_id_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidConfig("'client_id_ref' is required".to_owned()))?
            .to_owned();
        let client_secret_ref = config
            .get("client_secret_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidConfig("'client_secret_ref' is required".to_owned())
            })?
            .to_owned();
        Ok(Self {
            token_endpoint: token_endpoint.map(str::to_owned),
            issuer_url: issuer_url.map(str::to_owned),
            client_id_ref,
            client_secret_ref,
            scopes: config.get("scopes").and_then(Value::as_str).map(str::to_owned),
        })
    }

    fn cache_key(&self, tenant: Uuid, method: ClientAuthMethod) -> String {
        // Deterministic canonical serialization of the config.
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(te) = &self.token_endpoint {
            pairs.push(("token_endpoint".into(), te.clone()));
        }
        if let Some(iss) = &self.issuer_url {
            pairs.push(("issuer_url".into(), iss.clone()));
        }
        pairs.push(("client_id_ref".into(), self.client_id_ref.clone()));
        pairs.push(("client_secret_ref".into(), self.client_secret_ref.clone()));
        if let Some(sc) = &self.scopes {
            pairs.push(("scopes".into(), sc.clone()));
        }
        pairs.sort();
        let canonical = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{tenant}:{}:{canonical}", method.tag())
    }
}

/// OAuth2 Client Credentials auth plugin (RFC 6749 §4.4) with an internal
/// token cache. Both `oauth2_client_cred` (Form) and
/// `oauth2_client_cred_basic` (Basic) share this implementation — only the
/// credential transport differs (ADR 0008).
pub struct OAuth2ClientCredAuthPlugin {
    id: &'static str,
    method: ClientAuthMethod,
    secrets: Arc<dyn SecretResolver>,
    http: Arc<OagwHttpClient>,
    cache_ttl: Duration,
    cache_capacity: usize,
    cache: dashmap::DashMap<String, CachedToken>,
}

impl OAuth2ClientCredAuthPlugin {
    /// Build the plugin for one client-auth method.
    #[must_use]
    pub fn new(
        id: &'static str,
        method: ClientAuthMethod,
        secrets: Arc<dyn SecretResolver>,
        http: Arc<OagwHttpClient>,
        cache: TokenCacheConfig,
    ) -> Self {
        Self {
            id,
            method,
            secrets,
            http,
            cache_ttl: cache.ttl(),
            cache_capacity: cache.capacity(),
            cache: dashmap::DashMap::new(),
        }
    }

    async fn fetch_token(
        &self,
        tenant: Uuid,
        config: &OAuth2Config,
    ) -> Result<(String, u64), PluginError> {
        let endpoint = match &config.token_endpoint {
            Some(te) => te.clone(),
            None => self.discover_token_endpoint(tenant, config).await?,
        };
        let client_id = self.resolve_secret(tenant, &config.client_id_ref).await?;
        let client_secret = self.resolve_secret(tenant, &config.client_secret_ref).await?;

        let mut form: Vec<(String, String)> = vec![
            ("grant_type".to_owned(), "client_credentials".to_owned()),
            ("client_id".to_owned(), client_id.clone()),
            ("client_secret".to_owned(), client_secret.clone()),
        ];
        if let Some(scopes) = &config.scopes {
            form.push(("scope".to_owned(), scopes.clone()));
        }

        let mut builder = Request::builder().method(Method::POST).uri(&endpoint);
        let body = match self.method {
            ClientAuthMethod::Form => {
                builder = builder.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
                form_urlencoded_body(&form)
            }
            ClientAuthMethod::Basic => {
                // Basic auth: client_id/client_secret in Authorization header.
                let creds = base64_encode(&format!("{client_id}:{client_secret}"));
                builder = builder.header("authorization", format!("Basic {creds}"));
                builder = builder.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
                let mut basic = form
                    .into_iter()
                    .filter(|(k, _)| k != "client_id" && k != "client_secret")
                    .collect::<Vec<_>>();
                if config.scopes.is_none() {
                    // keep body minimal; grant_type always present
                }
                basic.push(("grant_type".to_owned(), "client_credentials".to_owned()));
                form_urlencoded_body(&basic)
            }
        };

        let req = builder
            .body(Body::from(body))
            .map_err(|e| PluginError::External(format!("token request build failed: {e}")))?;
        let resp = self
            .http
            .send(req)
            .await
            .map_err(|e| PluginError::External(format!("token endpoint request failed: {e}")))?;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .map_err(|e| PluginError::External(format!("token response read failed: {e}")))?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|e| PluginError::External(format!("token response is not JSON: {e}")))?;
        if !status.is_success() {
            return Err(PluginError::External(format!(
                "token endpoint returned {status}: {}",
                json.get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified OAuth2 error")
            )));
        }
        let token = json
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::External("token response lacks access_token".to_owned()))?
            .to_owned();
        let expires_in = json.get("expires_in").and_then(Value::as_u64).unwrap_or(300);
        Ok((token, expires_in))
    }

    async fn discover_token_endpoint(
        &self,
        _tenant: Uuid,
        config: &OAuth2Config,
    ) -> Result<String, PluginError> {
        let issuer = config
            .issuer_url
            .as_deref()
            .ok_or_else(|| PluginError::InvalidConfig("'issuer_url' is required".to_owned()))?;
        let discovery = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let req = Request::builder()
            .method(Method::GET)
            .uri(&discovery)
            .body(Body::empty())
            .map_err(|e| PluginError::External(format!("discovery request build failed: {e}")))?;
        let resp = self
            .http
            .send(req)
            .await
            .map_err(|e| PluginError::External(format!("OIDC discovery failed: {e}")))?;
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .map_err(|e| PluginError::External(format!("discovery read failed: {e}")))?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|e| PluginError::External(format!("discovery is not JSON: {e}")))?;
        json.get("token_endpoint")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| PluginError::External("OIDC discovery lacks token_endpoint".to_owned()))
    }

    async fn resolve_secret(&self, tenant: Uuid, reference: &str) -> Result<String, PluginError> {
        let bytes = self
            .secrets
            .resolve(tenant, reference)
            .await
            .map_err(|e| PluginError::External(format!("secret '{reference}' resolution failed: {e}")))?;
        String::from_utf8(bytes)
            .map_err(|_| PluginError::External(format!("secret '{reference}' is not valid UTF-8")))
    }
}

fn form_urlencoded_body(fields: &[(String, String)]) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&urlencoding(k, v));
    }
    out.into_bytes()
}

fn base64_encode(bytes: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = bytes.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn inject_credentials(
        &self,
        request: &mut ProxyRequestView,
        config: &Value,
        secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        let _ = secrets; // resolution is internal via self.secrets (tenant-scoped)
        let parsed = OAuth2Config::parse(config)?;
        let key = parsed.cache_key(request.tenant_id, self.method);
        let now = Instant::now();
        if let Some(entry) = self.cache.get(&key) {
            if entry.valid(&key, now) {
                request
                    .headers
                    .insert("authorization", HeaderValue::from_str(&format!("Bearer {}", entry.token)).expect("bearer"));
                return Ok(());
            }
        }
        let (token, expires_in) = self.fetch_token(request.tenant_id, &parsed).await?;
        // TTL = min(config ceiling, expires_in - 30s safety margin) (ADR 0008).
        let margin = expires_in.saturating_sub(30);
        let ttl = Duration::from_secs(margin).min(self.cache_ttl);
        // Respect capacity: evict nothing, simply don't cache when full.
        if self.cache.len() < self.cache_capacity || self.cache.contains_key(&key) {
            self.cache.insert(
                key.clone(),
                CachedToken {
                    key,
                    token: token.clone(),
                    cached_at: now,
                    ttl,
                },
            );
        }
        request
            .headers
            .insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer"),
            );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Guard: required_headers (ADR 0009)
// ---------------------------------------------------------------------------

/// Stateless presence guard. Config keys:
///
/// - `required_request_headers` — comma-separated names checked before
///   proxying (missing → 400).
/// - `required_response_headers` — comma-separated names checked on the
///   upstream response (missing → 502).
pub struct RequiredHeadersGuardPlugin;

fn parse_header_list(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_ascii_lowercase)
            .collect(),
        Value::Null => Vec::new(),
        _ => Vec::new(),
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        g::GUARD_REQUIRED_HEADERS
    }

    fn checks_request(&self) -> bool {
        true
    }

    fn checks_response(&self) -> bool {
        true
    }

    async fn check_request(
        &self,
        request: &ProxyRequestView,
        config: &Value,
    ) -> Result<(), PluginError> {
        for name in parse_header_list(&config.get("required_request_headers").unwrap_or(&Value::Null)) {
            if !request.headers.contains_key(&name) {
                return Err(validation_rejected(format!(
                    "required request header '{name}' is missing"
                )));
            }
        }
        Ok(())
    }

    async fn check_response(
        &self,
        response: &ProxyResponseView,
        config: &Value,
    ) -> Result<(), PluginError> {
        for name in parse_header_list(&config.get("required_response_headers").unwrap_or(&Value::Null)) {
            if !response.headers.contains_key(&name) {
                return Err(PluginError::rejected(
                    502,
                    g::ERR_DOWNSTREAM_ERROR,
                    "Downstream Error",
                    format!("required response header '{name}' is missing"),
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Transform: request_id
// ---------------------------------------------------------------------------

/// `X-Request-ID` injection/propagation on requests and responses.
pub struct RequestIdTransformPlugin;

const REQUEST_ID_HEADER: &str = "x-request-id";

fn ensure_request_id(headers: &mut http::HeaderMap) {
    if !headers.contains_key(REQUEST_ID_HEADER) {
        if let Ok(v) = HeaderValue::from_str(&Uuid::new_v4().to_string()) {
            headers.insert(REQUEST_ID_HEADER, v);
        }
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        g::TRANSFORM_REQUEST_ID
    }

    async fn on_request(
        &self,
        request: &mut ProxyRequestView,
        _config: &Value,
    ) -> Result<(), PluginError> {
        ensure_request_id(&mut request.headers);
        Ok(())
    }

    async fn on_response(
        &self,
        response: &mut ProxyResponseView,
        _config: &Value,
    ) -> Result<(), PluginError> {
        ensure_request_id(&mut response.headers);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::domain::plugin::auth_failed;

    fn view(tenant: Uuid) -> ProxyRequestView {
        ProxyRequestView {
            method: Method::GET,
            path: "/".into(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            tenant_id: tenant,
            body_length_hint: None,
        }
    }

    struct StaticSecretResolver(HashMap<String, String>);
    #[async_trait]
    impl SecretResolver for StaticSecretResolver {
        async fn resolve(
            &self,
            _tenant_id: Uuid,
            secret_ref: &str,
        ) -> Result<Vec<u8>, crate::domain::error::DomainError> {
            self.0
                .get(secret_ref)
                .map(|v| v.clone().into_bytes())
                .ok_or_else(|| crate::domain::error::DomainError::SecretNotFound(secret_ref.into()))
        }
    }

    #[tokio::test]
    async fn apikey_injects_header_and_query() {
        let plugin = ApiKeyAuthPlugin;
        let mut v = view(Uuid::new_v4());
        let config = serde_json::json!({
            "header": "X-Api-Key",
            "query_param": "apiKey",
            "key": "sekret",
        });
        plugin
            .inject_credentials(&mut v, &config, &StaticSecretResolver(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(v.headers.get("x-api-key").unwrap(), "sekret");
        assert!(v.query.contains("apiKey=sekret"));
    }

    #[tokio::test]
    async fn apikey_requires_key_or_key_ref() {
        let plugin = ApiKeyAuthPlugin;
        let mut v = view(Uuid::new_v4());
        let config = serde_json::json!({ "header": "X-Api-Key" });
        assert!(matches!(
            plugin
                .inject_credentials(&mut v, &config, &StaticSecretResolver(HashMap::new()))
                .await,
            Err(PluginError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn apikey_resolves_via_secret_ref() {
        let plugin = ApiKeyAuthPlugin;
        let mut v = view(Uuid::new_v4());
        let config = serde_json::json!({ "key_ref": "cred://partner-key" });
        // The resolver receives the full `cred://` reference (the production
        // CredStoreSecretResolver strips the prefix when talking to the
        // store).
        let secrets = StaticSecretResolver(HashMap::from([(
            "cred://partner-key".to_owned(),
            "from-store".to_owned(),
        )]));
        plugin.inject_credentials(&mut v, &config, &secrets).await.unwrap();
        assert_eq!(v.headers.get("x-api-key").unwrap(), "from-store");
    }

    #[tokio::test]
    async fn noop_injects_nothing() {
        let plugin = NoopAuthPlugin;
        let mut v = view(Uuid::new_v4());
        plugin
            .inject_credentials(&mut v, &serde_json::Value::Null, &StaticSecretResolver(HashMap::new()))
            .await
            .unwrap();
        assert!(v.headers.is_empty());
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_missing_request_header() {
        let plugin = RequiredHeadersGuardPlugin;
        let v = view(Uuid::new_v4());
        let config = serde_json::json!({ "required_request_headers": "x-correlation-id, accept" });
        let err = plugin.check_request(&v, &config).await.unwrap_err();
        match err {
            PluginError::Rejected(r) => {
                assert_eq!(r.status, 400);
                assert!(r.detail.contains("x-correlation-id"));
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn required_headers_guard_fail_open_on_blank_config() {
        let plugin = RequiredHeadersGuardPlugin;
        let v = view(Uuid::new_v4());
        let config = serde_json::json!({ "required_request_headers": " , , " });
        plugin.check_request(&v, &config).await.unwrap();
    }

    #[tokio::test]
    async fn required_headers_guard_passes_when_present() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut v = view(Uuid::new_v4());
        v.headers.insert("x-correlation-id", HeaderValue::from_static("abc"));
        let config = serde_json::json!({ "required_request_headers": "x-correlation-id" });
        plugin.check_request(&v, &config).await.unwrap();
    }

    #[tokio::test]
    async fn required_response_headers_rejects_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let resp = ProxyResponseView {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body_len: 0,
        };
        let config = serde_json::json!({ "required_response_headers": "content-type" });
        let err = plugin.check_response(&resp, &config).await.unwrap_err();
        match err {
            PluginError::Rejected(r) => assert_eq!(r.status, 502),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_id_injects_and_preserves() {
        let plugin = RequestIdTransformPlugin;
        let mut v = view(Uuid::new_v4());
        plugin.on_request(&mut v, &Value::Null).await.unwrap();
        let id = v.headers.get(REQUEST_ID_HEADER).unwrap().to_str().unwrap().to_owned();
        assert!(!id.is_empty());
        let mut resp = ProxyResponseView {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body_len: 0,
        };
        plugin.on_response(&mut resp, &Value::Null).await.unwrap();
        assert!(resp.headers.contains_key(REQUEST_ID_HEADER));
    }

    #[tokio::test]
    async fn oauth2_config_validation() {
        // both endpoints → InvalidConfig
        let cfg = serde_json::json!({
            "token_endpoint": "https://idp.example/token",
            "issuer_url": "https://idp.example",
            "client_id_ref": "cred://c",
            "client_secret_ref": "cred://s",
        });
        assert!(matches!(
            OAuth2Config::parse(&cfg),
            Err(PluginError::InvalidConfig(_))
        ));
        // neither → InvalidConfig
        let cfg = serde_json::json!({ "client_id_ref": "cred://c", "client_secret_ref": "cred://s" });
        assert!(matches!(
            OAuth2Config::parse(&cfg),
            Err(PluginError::InvalidConfig(_))
        ));
        // missing secret ref → InvalidConfig
        let cfg = serde_json::json!({
            "token_endpoint": "https://idp.example/token",
            "client_id_ref": "cred://c",
        });
        assert!(matches!(
            OAuth2Config::parse(&cfg),
            Err(PluginError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn oauth2_exchanges_credentials_for_bearer() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let token_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .header("authorization", "Basic Y2xpZW50OnNlY3JldA==");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-123","expires_in":3600}"#);
        });

        let http = Arc::new(OagwHttpClient::new());
        let plugin = OAuth2ClientCredAuthPlugin::new(
            g::AUTH_OAUTH2_CLIENT_CRED_BASIC,
            ClientAuthMethod::Basic,
            Arc::new(StaticSecretResolver(HashMap::from([
                ("cred://client".to_owned(), "client".to_owned()),
                ("cred://secret".to_owned(), "secret".to_owned()),
            ]))),
            http,
            TokenCacheConfig::default(),
        );
        let mut v = view(Uuid::new_v4());
        let config = serde_json::json!({
            "token_endpoint": format!("{}/token", server.base_url()),
            "client_id_ref": "cred://client",
            "client_secret_ref": "cred://secret",
        });
        plugin
            .inject_credentials(&mut v, &config, &StaticSecretResolver(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(v.headers.get("authorization").unwrap(), "Bearer tok-123");
        token_mock.assert();
        // Second call hits the cache — no second fetch.
        plugin
            .inject_credentials(&mut v, &config, &StaticSecretResolver(HashMap::new()))
            .await
            .unwrap();
        token_mock.assert_calls(1);
    }

    #[test]
    fn cache_key_partitions_by_tenant_and_method() {
        let cfg = OAuth2Config {
            token_endpoint: Some("https://idp.example/token".into()),
            issuer_url: None,
            client_id_ref: "cred://c".into(),
            client_secret_ref: "cred://s".into(),
            scopes: None,
        };
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert!(cfg.cache_key(a, ClientAuthMethod::Form) != cfg.cache_key(b, ClientAuthMethod::Form));
        assert!(cfg.cache_key(a, ClientAuthMethod::Form) != cfg.cache_key(a, ClientAuthMethod::Basic));
    }

    #[tokio::test]
    async fn auth_failed_helper_shape() {
        let err = auth_failed("bad token");
        match err {
            PluginError::Rejected(r) => assert_eq!(r.status, 401),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_id_preserves_inbound_id() {
        let plugin = RequestIdTransformPlugin;
        let mut v = view(Uuid::new_v4());
        v.headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("inbound-123"));
        plugin.on_request(&mut v, &Value::Null).await.unwrap();
        assert_eq!(v.headers.get(REQUEST_ID_HEADER).unwrap(), "inbound-123");
        // Response side also preserves an existing id and injects when absent.
        let mut resp = ProxyResponseView {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body_len: 0,
        };
        plugin.on_response(&mut resp, &Value::Null).await.unwrap();
        let injected = resp
            .headers
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(!injected.is_empty());
        resp.headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("upstream-id"));
        plugin.on_response(&mut resp, &Value::Null).await.unwrap();
        assert_eq!(resp.headers.get(REQUEST_ID_HEADER).unwrap(), "upstream-id");
    }
}
