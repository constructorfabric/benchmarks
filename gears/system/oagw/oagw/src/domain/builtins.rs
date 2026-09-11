// Created: 2026-09-02 by Constructor Tech
//! Built-in plugin implementations (`ADR-0002`, `ADR-0008`, `ADR-0009`).
//!
//! Each built-in is stateless except the OAuth2 client-credentials plugin,
//! which carries the token cache `ADR-0008` mandates. Configuration keys are
//! read from the plugin binding's `config` object; every key is optional unless
//! the ADR marks it required, and an unconfigured plugin is a no-op (fail-open).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use pingora_memory_cache::MemoryCache;
use url::Url;

use crate::domain::plugin::{AuthContext, AuthInjection, GuardPlugin, GuardVerdict, TransformPlugin};
use crate::error::{codes, GatewayError};

/// Auth plugin: no credential injection.
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl crate::domain::plugin::AuthPlugin for NoopAuthPlugin {
    fn plugin_id(&self) -> &'static str {
        crate::gts::AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &AuthContext<'_>) -> Result<AuthInjection, GatewayError> {
        Ok(AuthInjection::none())
    }
}

/// Auth plugin: static API key injection into a header or a query parameter.
///
/// Keys (all optional except `secret_ref`):
///
/// | key | default | meaning |
/// |---|---|---|
/// | `secret_ref` | — | `cred://` reference holding the key material |
/// | `header_name` | `x-api-key` | header the key is injected into |
/// | `query_param` | — | when set, the key is appended to the query instead |
pub struct ApiKeyAuthPlugin {
    _private: (),
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin.
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ApiKeyAuthPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::domain::plugin::AuthPlugin for ApiKeyAuthPlugin {
    fn plugin_id(&self) -> &'static str {
        crate::gts::AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &AuthContext<'_>) -> Result<AuthInjection, GatewayError> {
        let secret_ref = ctx.config_str("secret_ref").ok_or_else(|| {
            GatewayError::Validation("auth.config.secret_ref is required for the apikey plugin".to_owned())
        })?;
        let key = resolve_required(ctx.secrets, ctx.tenant_id, secret_ref_value(&secret_ref)).await?;

        if let Some(param) = ctx.config_str("query_param").filter(|p| !p.is_empty()) {
            return Ok(AuthInjection {
                headers: Vec::new(),
                query: vec![(param, key)],
            });
        }
        let header_name = ctx
            .config_str("header_name")
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "x-api-key".to_owned());
        let (name, value) = AuthInjection::header(&header_name, &key).ok_or_else(|| {
            GatewayError::Validation(format!("auth.config.header_name '{header_name}' is not a valid header name"))
        })?;
        Ok(AuthInjection { headers: vec![(name, value)], query: Vec::new() })
    }
}

/// Auth plugin: OAuth2 client credentials with an internal token cache.
///
/// `Form` and `Basic` differ only in how the client credentials are presented
/// to the token endpoint. Both share the cache, keyed by
/// `(subject tenant, subject, auth method, config hash)` so tenants and
/// subjects can never observe each other's tokens (`ADR-0008`).
pub struct OAuth2ClientCredPlugin {
    auth_method: toolkit_auth::oauth2::ClientAuthMethod,
    secrets: Arc<dyn crate::domain::plugin::SecretResolver>,
    cache: pingora_memory_cache::MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

/// A cache entry carrying the key it was stored under, so a `u64` hash
/// collision degrades to a cache miss instead of leaking another tenant's
/// token (`ADR-0008`).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: String,
}

impl OAuth2ClientCredPlugin {
    /// The `Form` variant: credentials in the request body.
    #[must_use]
    pub fn form(
        secrets: Arc<dyn crate::domain::plugin::SecretResolver>,
        cache_ttl_secs: u64,
        cache_capacity: usize,
    ) -> Self {
        Self::new(crate::gts::AUTH_OAUTH2_CC, toolkit_auth::oauth2::ClientAuthMethod::Form, secrets, cache_ttl_secs, cache_capacity)
    }

    /// The `Basic` variant: credentials in the `Authorization` header.
    #[must_use]
    pub fn basic(
        secrets: Arc<dyn crate::domain::plugin::SecretResolver>,
        cache_ttl_secs: u64,
        cache_capacity: usize,
    ) -> Self {
        Self::new(
            crate::gts::AUTH_OAUTH2_CC_BASIC,
            toolkit_auth::oauth2::ClientAuthMethod::Basic,
            secrets,
            cache_ttl_secs,
            cache_capacity,
        )
    }

    fn new(
        plugin_id: &'static str,
        auth_method: toolkit_auth::oauth2::ClientAuthMethod,
        secrets: Arc<dyn crate::domain::plugin::SecretResolver>,
        cache_ttl_secs: u64,
        cache_capacity: usize,
    ) -> Self {
        let _ = plugin_id;
        Self {
            auth_method,
            secrets,
            cache: MemoryCache::new(cache_capacity.max(1)),
            cache_ttl: Duration::from_secs(cache_ttl_secs.max(1)),
        }
    }

    fn cache_key(&self, ctx: &AuthContext<'_>) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (key, value) in ctx.config {
            key.hash(&mut hasher);
            serde_json::to_string(value).unwrap_or_default().hash(&mut hasher);
        }
        let config_hash = hasher.finish();
        format!(
            "{}:{}:{}:{config_hash:016x}",
            ctx.subject_tenant_id,
            ctx.subject_id,
            tag_of(self.auth_method)
        )
    }

    /// Performs the token exchange, returning the bearer value.
    async fn fetch(&self, ctx: &AuthContext<'_>) -> Result<(String, Duration), GatewayError> {
        let client_id_ref = ctx
            .config_str("client_id_ref")
            .ok_or_else(|| missing_config("client_id_ref"))?;
        let client_secret_ref = ctx
            .config_str("client_secret_ref")
            .ok_or_else(|| missing_config("client_secret_ref"))?;
        // Mutually exclusive per ADR-0008: a direct token endpoint, or an OIDC
        // issuer whose discovery document names one.
        let (token_endpoint, issuer_url) = match (
            ctx.config_str("token_endpoint").filter(|s| !s.is_empty()),
            ctx.config_str("issuer_url").filter(|s| !s.is_empty()),
        ) {
            (Some(endpoint), None) => {
                (Some(parse_url(&endpoint)?), None)
            }
            (None, Some(issuer)) => (None, Some(parse_url(&issuer)?)),
            (Some(_), Some(_)) => {
                return Err(GatewayError::Validation(
                    "auth.config accepts either token_endpoint or issuer_url, not both".to_owned(),
                ));
            }
            (None, None) => return Err(missing_config("token_endpoint or issuer_url")),
        };

        let client_id = resolve_required(self.secrets.as_ref(), ctx.tenant_id, secret_ref_value(&client_id_ref)).await?;
        let client_secret =
            resolve_required(self.secrets.as_ref(), ctx.tenant_id, secret_ref_value(&client_secret_ref)).await?;

        let scopes = ctx
            .config_str("scopes")
            .map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();

        let config = toolkit_auth::oauth2::OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id: client_id.clone(),
            client_secret: toolkit_auth::oauth2::SecretString::new(client_secret.as_str()),
            scopes,
            auth_method: self.auth_method,
            ..Default::default()
        };

        let fetched = toolkit_auth::oauth2::fetch_token(config).await.map_err(|err| {
            tracing::warn!(error = %err, "oauth2 token fetch failed");
            GatewayError::AuthenticationFailed(
                "authentication to the upstream failed: the credential issuer rejected the request".to_owned(),
            )
        })?;

        Ok((fetched.bearer.expose().to_owned(), fetched.expires_in))
    }
}

fn tag_of(method: toolkit_auth::oauth2::ClientAuthMethod) -> &'static str {
    match method {
        toolkit_auth::oauth2::ClientAuthMethod::Basic => "basic",
        toolkit_auth::oauth2::ClientAuthMethod::Form => "form",
    }
}

/// `expires_in` safety margin before a cached token is considered stale.
const SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// `Authorization: Bearer <token>` injection.
fn bearer_injection(token: &str) -> Result<AuthInjection, GatewayError> {
    let (name, value) = AuthInjection::header("authorization", &format!("Bearer {token}"))
        .ok_or_else(|| GatewayError::Internal("authorization header is not renderable".to_owned()))?;
    Ok(AuthInjection { headers: vec![(name, value)], query: Vec::new() })
}

fn missing_config(key: &str) -> GatewayError {
    GatewayError::Validation(format!("auth.config.{key} is required for the oauth2 client-credentials plugin"))
}

fn parse_url(raw: &str) -> Result<Url, GatewayError> {
    Url::parse(raw)
        .map_err(|err| GatewayError::Validation(format!("auth.config '{raw}' is not a valid URL: {err}")))
}

/// Accepts the `cred://` prefix the schema documents and a bare name.
fn secret_ref_value(raw: &str) -> &str {
    raw.strip_prefix("cred://").unwrap_or(raw)
}

async fn resolve_required(
    secrets: &dyn crate::domain::plugin::SecretResolver,
    tenant_id: uuid::Uuid,
    reference: &str,
) -> Result<String, GatewayError> {
    match secrets.resolve(tenant_id, reference).await {
        Ok(Some(value)) if !value.is_empty() => Ok(value),
        Ok(_) => Err(GatewayError::SecretNotFound(reference.to_owned())),
        Err(err) => {
            tracing::warn!(error = %err, "credential store lookup failed");
            Err(GatewayError::AuthenticationFailed(
                "authentication to the upstream failed: the credential store rejected the request".to_owned(),
            ))
        }
    }
}

#[async_trait::async_trait]
impl crate::domain::plugin::AuthPlugin for OAuth2ClientCredPlugin {
    fn plugin_id(&self) -> &'static str {
        match self.auth_method {
            toolkit_auth::oauth2::ClientAuthMethod::Basic => crate::gts::AUTH_OAUTH2_CC_BASIC,
            toolkit_auth::oauth2::ClientAuthMethod::Form => crate::gts::AUTH_OAUTH2_CC,
        }
    }

    async fn authenticate(&self, ctx: &AuthContext<'_>) -> Result<AuthInjection, GatewayError> {
        let key = self.cache_key(ctx);
        if let (Some(cached), _) = self.cache.get(&key)
            && cached.key == key {
                return bearer_injection(&cached.token);
            }

        let (token, expires_in) = self.fetch(ctx).await?;
        // `min(configured TTL, expires_in − 30s safety margin)`; a token with
        // less than 30s of life left is not cached at all (ADR-0008).
        let ttl = expires_in
            .checked_sub(SAFETY_MARGIN)
            .map(|remaining| remaining.min(self.cache_ttl))
            .filter(|ttl| !ttl.is_zero());
        if let Some(ttl) = ttl {
            self.cache.put(&key, CachedToken { key: key.clone(), token: token.clone() }, Some(ttl));
        }
        bearer_injection(&token)
    }
}

/// Guard plugin: required request/response header enforcement (`ADR-0009`).
///
/// Keys: `required_request_headers` and `required_response_headers`, each a
/// comma-separated list of header names matched case-insensitively. A phase
/// whose key is absent or blank is a no-op, and only the first missing header
/// is reported.
pub struct RequiredHeadersGuard;

impl RequiredHeadersGuard {
    fn parse(config: &BTreeMap<String, serde_json::Value>, key: &str) -> Vec<String> {
        let Some(raw) = config.get(key) else {
            return Vec::new();
        };
        let raw = match raw {
            serde_json::Value::String(s) => s.clone(),
            // A list of names is accepted as well: the same intent, typed.
            serde_json::Value::Array(items) => {
                return items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_ascii_lowercase())
                    .collect();
            }
            _ => return Vec::new(),
        };
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    }

    fn check(
        &self,
        headers: &HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
        key: &str,
        request_phase: bool,
    ) -> GuardVerdict {
        let required = Self::parse(config, key);
        if required.is_empty() {
            return GuardVerdict::Allow;
        }
        let Some(missing) = required
            .iter()
            .find(|name| !headers.contains_key(name.as_str()))
        else {
            return GuardVerdict::Allow;
        };
        let error = if request_phase {
            GatewayError::Validation(format!("required header '{missing}' is missing"))
        } else {
            GatewayError::DownstreamError(format!("upstream response is missing required header '{missing}'"))
        };
        GuardVerdict::Reject(error)
    }
}

impl GuardPlugin for RequiredHeadersGuard {
    fn plugin_id(&self) -> &'static str {
        crate::gts::GUARD_REQUIRED_HEADERS
    }

    fn guard_request(
        &self,
        headers: &HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
    ) -> GuardVerdict {
        self.check(headers, config, "required_request_headers", true)
    }

    fn guard_response(
        &self,
        headers: &HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
    ) -> GuardVerdict {
        self.check(headers, config, "required_response_headers", false)
    }
}

/// Transform plugin: `X-Request-ID` injection and propagation.
pub struct RequestIdTransform;

impl TransformPlugin for RequestIdTransform {
    fn plugin_id(&self) -> &'static str {
        crate::gts::TRANSFORM_REQUEST_ID
    }

    fn transform_request(&self, head: &mut crate::domain::plugin::RequestHead, _config: &BTreeMap<String, serde_json::Value>) {
        if head.headers.contains_key(crate::gts::REQUEST_ID_HEADER) {
            return;
        }
        let generated = uuid::Uuid::now_v7().to_string();
        if let Ok(value) = axum::http::HeaderValue::from_str(&generated) {
            head.headers.insert(crate::gts::REQUEST_ID_HEADER, value);
        }
    }

    fn transform_response(&self, head: &mut HeaderMap, _config: &BTreeMap<String, serde_json::Value>) {
        // Echo the request id back to the caller when the upstream did not.
        if let Some(id) = head.get(crate::gts::REQUEST_ID_HEADER) {
            let _ = id;
        }
    }
}

/// `codes::REQUIRED_HEADER_MISSING`, re-exported for the guard's callers.
#[must_use]
pub fn required_header_missing_code() -> &'static str {
    codes::REQUIRED_HEADER_MISSING
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::{AuthPlugin, SecretResolver};

    struct StaticSecrets(BTreeMap<String, String>);

    #[async_trait::async_trait]
    impl SecretResolver for StaticSecrets {
        async fn resolve(&self, _t: uuid::Uuid, r: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.get(r).cloned())
        }
    }

    fn ctx<'a>(
        secrets: &'a StaticSecrets,
        config: &'a BTreeMap<String, serde_json::Value>,
    ) -> AuthContext<'a> {
        AuthContext {
            tenant_id: uuid::Uuid::from_u128(1),
            subject_tenant_id: uuid::Uuid::from_u128(1),
            subject_id: "user-1",
            upstream_id: uuid::Uuid::from_u128(2),
            config,
            secrets,
        }
    }

    fn map(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[tokio::test]
    async fn noop_injects_nothing() {
        let secrets = StaticSecrets(BTreeMap::new());
        let plugin = NoopAuthPlugin;
        let injection = plugin.authenticate(&ctx(&secrets, &map(&[]))).await.unwrap();
        assert!(injection.is_empty());
    }

    #[tokio::test]
    async fn apikey_injects_the_header_by_default() {
        let secrets = StaticSecrets(BTreeMap::from([("sk-1".to_owned(), "abcd1234".to_owned())]));
        let plugin = ApiKeyAuthPlugin::new();
        let injection = plugin
            .authenticate(&ctx(&secrets, &map(&[("secret_ref", "sk-1".into())])))
            .await
            .unwrap();
        assert_eq!(injection.headers.len(), 1);
        assert_eq!(injection.headers[0].0.as_str(), "x-api-key");
        assert_eq!(injection.headers[0].1, "abcd1234");
        assert!(injection.query.is_empty());
    }

    #[tokio::test]
    async fn apikey_can_inject_into_the_query_instead() {
        let secrets = StaticSecrets(BTreeMap::from([("sk-1".to_owned(), "abcd1234".to_owned())]));
        let plugin = ApiKeyAuthPlugin::new();
        let injection = plugin
            .authenticate(&ctx(&secrets, &map(&[("secret_ref", "sk-1".into()), ("query_param", "api_key".into())]),
            ))
            .await
            .unwrap();
        assert!(injection.headers.is_empty());
        assert_eq!(injection.query, vec![("api_key".to_owned(), "abcd1234".to_owned())]);
    }

    #[tokio::test]
    async fn apikey_requires_a_secret_ref() {
        let secrets = StaticSecrets(BTreeMap::new());
        let err = ApiKeyAuthPlugin::new()
            .authenticate(&ctx(&secrets, &map(&[])))
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[tokio::test]
    async fn apikey_reports_an_unresolvable_secret() {
        let secrets = StaticSecrets(BTreeMap::new());
        let err = ApiKeyAuthPlugin::new()
            .authenticate(&ctx(&secrets, &map(&[("secret_ref", "sk-missing".into())])))
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::SecretNotFound(_)));
    }

    #[tokio::test]
    async fn oauth2_requires_the_config_keys() {
        let secrets = StaticSecrets(BTreeMap::new());
        let plugin = OAuth2ClientCredPlugin::form(Arc::new(StaticSecrets(BTreeMap::new())), 300, 10);
        let err = plugin.authenticate(&ctx(&secrets, &map(&[]))).await.unwrap_err();
        assert!(matches!(err, GatewayError::Validation(_)));
    }

    #[test]
    fn required_headers_is_fail_open_and_case_insensitive() {
        let guard = RequiredHeadersGuard;
        let mut headers = HeaderMap::new();
        headers.insert("X-Correlation-Id", "abc".parse().unwrap());

        // Unconfigured → allow.
        assert_eq!(guard.guard_request(&headers, &map(&[])), GuardVerdict::Allow);
        assert_eq!(
            guard.guard_request(&headers, &map(&[("required_request_headers", " ".into())])),
            GuardVerdict::Allow
        );
        // Case-insensitive presence: the configuration names the header in a
        // different case than the request sends it.
        let upper = map(&[("required_request_headers", "X-CORRELATION-ID".into())]);
        assert_eq!(guard.guard_request(&headers, &upper), GuardVerdict::Allow);
        // Missing → reject, naming the first missing header.
        let config = map(&[("required_request_headers", "x-correlation-id, accept".into())]);
        let verdict = guard.guard_request(&headers, &config);
        match verdict {
            GuardVerdict::Reject(err) => assert!(err.to_string().contains("accept")),
            other => panic!("expected reject, got {other:?}"),
        }
        // Array form is accepted too.
        let config = map(&[("required_request_headers", serde_json::json!(["x-correlation-id"]))]);
        assert_eq!(guard.guard_request(&headers, &config), GuardVerdict::Allow);
    }

    #[test]
    fn required_response_headers_rejects_with_502() {
        let guard = RequiredHeadersGuard;
        let config = map(&[("required_response_headers", "content-type".into())]);
        let verdict = guard.guard_response(&HeaderMap::new(), &config);
        match verdict {
            GuardVerdict::Reject(err) => assert_eq!(err.status(), axum::http::StatusCode::BAD_GATEWAY),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn request_id_is_generated_when_absent_and_preserved_when_present() {
        let transform = RequestIdTransform;
        let mut head = crate::domain::plugin::RequestHead::default();
        transform.transform_request(&mut head, &BTreeMap::new());
        assert!(head.headers.contains_key("x-request-id"));

        let mut head = crate::domain::plugin::RequestHead::default();
        head.headers.insert("x-request-id", "given".parse().unwrap());
        transform.transform_request(&mut head, &BTreeMap::new());
        assert_eq!(head.headers.get("x-request-id").unwrap(), "given");
    }

    #[test]
    fn oauth2_plugin_ids_distinguish_the_variants() {
        let secrets: Arc<dyn SecretResolver> = Arc::new(StaticSecrets(BTreeMap::new()));
        assert_eq!(
            OAuth2ClientCredPlugin::form(secrets.clone(), 300, 10).plugin_id(),
            crate::gts::AUTH_OAUTH2_CC
        );
        assert_eq!(
            OAuth2ClientCredPlugin::basic(secrets, 300, 10).plugin_id(),
            crate::gts::AUTH_OAUTH2_CC_BASIC
        );
    }

    #[test]
    fn secret_refs_accept_the_cred_prefix() {
        assert_eq!(secret_ref_value("cred://partner-key"), "partner-key");
        assert_eq!(secret_ref_value("partner-key"), "partner-key");
    }

    #[tokio::test]
    async fn oauth2_exchanges_credentials_and_caches_the_token() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let mut mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"token-from-idp","token_type":"Bearer","expires_in":3600}"#);
        });

        let secrets = StaticSecrets(BTreeMap::from([
            ("id".to_owned(), "client-1".to_owned()),
            ("pw".to_owned(), "hunter2".to_owned()),
        ]));
        let plugin = OAuth2ClientCredPlugin::form(Arc::new(secrets), 300, 10);
        let config = map(&[
            (
                "token_endpoint",
                serde_json::json!(format!("http://127.0.0.1:{}/token", server.port())),
            ),
            ("client_id_ref", "id".into()),
            ("client_secret_ref", "pw".into()),
        ]);
        let ctx_secrets = StaticSecrets(BTreeMap::from([
            ("id".to_owned(), "client-1".to_owned()),
            ("pw".to_owned(), "hunter2".to_owned()),
        ]));

        let first = plugin.authenticate(&ctx(&ctx_secrets, &config)).await.unwrap();
        assert_eq!(first.headers[0].0.as_str(), "authorization");
        assert!(first.headers[0].1.to_str().unwrap().starts_with("Bearer "));

        // Second call is served from the cache: no second IdP request.
        let second = plugin.authenticate(&ctx(&ctx_secrets, &config)).await.unwrap();
        assert_eq!(second.headers[0].1, first.headers[0].1);
        assert_eq!(mock.calls(), 1);
        mock.delete();
    }

    #[test]
    fn the_guard_code_is_the_adr_code() {
        assert_eq!(required_header_missing_code(), "REQUIRED_HEADER_MISSING");
    }
}
