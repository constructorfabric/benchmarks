//! Client-credentials token acquisition
//! (`cpt-cf-oagw-algo-plugin-token-acquire`,
//! `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`).
//!
//! Parses the two client-credentials plugin variants' shared `ctx.config`
//! surface, composes the tenant/subject/method/config cache key, serves a
//! verified cache hit, and otherwise resolves the client credentials via
//! [`super::credential::resolve_secret`] and performs a single one-shot
//! token exchange through `toolkit_auth::oauth2::fetch_token` -- which
//! already implements the `Form`/`Basic` client-auth methods and OIDC
//! Discovery this feature's contract requires, so it is reused rather than
//! reimplemented.
//!
//! RF-001: reached for real from `crate::proxy::engine` (through
//! `super::auth`'s production code, in turn reached from `super::execute`'s
//! chain executor) whenever a request's merged `AuthConfig` names
//! `oauth2_client_cred`/`oauth2_client_cred_basic`.

use serde_json::Value;
use toolkit_auth::oauth2::{
    ClientAuthMethod, OAuthClientConfig, SecretString, TokenError, fetch_token,
};
use toolkit_http::HttpClientConfig;
use toolkit_security::SecurityContext;
use url::Url;

use credstore_sdk::CredStoreClientV1;

use super::credential::{CredentialError, resolve_secret};
use super::token_cache::{TokenCache, effective_ttl_secs};

const AUTH_METHOD_TAG_FORM: &str = "form";
const AUTH_METHOD_TAG_BASIC: &str = "basic";

/// A token-acquisition failure -- every variant maps to `401
/// AuthenticationFailed` and every rendered message is built from
/// [`TokenAcquireError::safe_detail`], never from the raw values below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenAcquireError {
    /// Both `token_endpoint`/`issuer_url` present, or neither
    /// (`inst-token-acquire-02`/`-03`).
    ExclusiveOrViolation,
    /// `client_id_ref`/`client_secret_ref` missing (`inst-token-acquire-04`/`-05`).
    MissingKey(&'static str),
    /// `token_endpoint`/`issuer_url` present but not a valid URL.
    InvalidEndpoint,
    Credential(CredentialError),
    /// Discovery/transport/non-success token-endpoint response
    /// (`inst-token-acquire-14`/`-15`). The inner string is
    /// `toolkit_auth`'s own sanitized `TokenError` rendering, which never
    /// contains secrets or a raw response body.
    Fetch(String),
}

impl TokenAcquireError {
    /// A message safe to place in an RFC 9457 `detail`, a log line, or an
    /// audit record (`cpt-cf-oagw-dod-plugin-secret-nondisclosure`).
    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::ExclusiveOrViolation => {
                "exactly one of token_endpoint/issuer_url must be configured".to_owned()
            }
            Self::MissingKey(key) => format!("missing required oauth2 config key: {key}"),
            Self::InvalidEndpoint => "token_endpoint/issuer_url is not a valid URL".to_owned(),
            Self::Credential(_) => "client credential could not be resolved".to_owned(),
            Self::Fetch(_) => "token exchange with the identity provider failed".to_owned(),
        }
    }
}

fn method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => AUTH_METHOD_TAG_FORM,
        ClientAuthMethod::Basic => AUTH_METHOD_TAG_BASIC,
    }
}

/// Deterministic canonicalization of a JSON value with all object keys
/// sorted, used only to build the cache-key's config hash
/// (`inst-token-acquire-06`) -- never logged, never rendered to a caller.
fn canonicalize(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{k}={}", canonicalize(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonicalize).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn hash_config(config: &Value) -> u64 {
    TokenCache::key_hash(&canonicalize(config))
}

/// `inst-token-acquire-06`: `subject_tenant_id : subject_id : auth_method
/// : sorted-config-hash`.
pub(crate) fn build_cache_key(
    ctx: &SecurityContext,
    method: ClientAuthMethod,
    config: &Value,
) -> String {
    format!(
        "{}:{}:{}:{}",
        ctx.subject_tenant_id(),
        ctx.subject_id(),
        method_tag(method),
        hash_config(config),
    )
}

#[derive(Debug)]
struct ParsedConfig {
    token_endpoint: Option<String>,
    issuer_url: Option<String>,
    client_id_ref: String,
    client_secret_ref: String,
    scopes: Option<String>,
}

fn config_str(config: &Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `inst-token-acquire-01` through `-05`: parse and validate the
/// `ctx.config` surface before any `cred_store` lookup or network call.
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-03
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-04
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-05
// @cpt-dod:cpt-cf-oagw-dod-plugin-oauth2-variants:p1
fn parse_config(config: &Value) -> Result<ParsedConfig, TokenAcquireError> {
    let token_endpoint = config_str(config, "token_endpoint");
    let issuer_url = config_str(config, "issuer_url");
    match (&token_endpoint, &issuer_url) {
        (Some(_), Some(_)) | (None, None) => return Err(TokenAcquireError::ExclusiveOrViolation),
        _ => {}
    }
    let client_id_ref = config_str(config, "client_id_ref")
        .ok_or(TokenAcquireError::MissingKey("client_id_ref"))?;
    let client_secret_ref = config_str(config, "client_secret_ref")
        .ok_or(TokenAcquireError::MissingKey("client_secret_ref"))?;
    let scopes = config_str(config, "scopes");
    Ok(ParsedConfig {
        token_endpoint,
        issuer_url,
        client_id_ref,
        client_secret_ref,
        scopes,
    })
}
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-05
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-04
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-03
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-02
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-01

/// Bound the outbound discovery/token-exchange call by the gear's
/// `proxy_timeout_secs`, per `cpt-cf-oagw-dod-plugin-oauth2-variants`; no
/// retry (`retry: None`) so the call cannot exceed this bound through
/// retry-induced delay.
fn bounded_http_config(proxy_timeout_secs: u32) -> HttpClientConfig {
    let timeout = std::time::Duration::from_secs(u64::from(proxy_timeout_secs.max(1)));
    // `HttpClientConfig` is `#[non_exhaustive]` in `toolkit-http`, so it
    // cannot be built with struct-update syntax from outside that crate;
    // start from `minimal()` (no retry, so a retry can never push the call
    // past `proxy_timeout_secs`) and override the two timeout fields.
    let mut config = HttpClientConfig::minimal();
    config.request_timeout = timeout;
    config.total_timeout = Some(timeout);
    config
}

/// Acquire a client-credentials bearer token, serving a verified cache hit
/// when one exists and otherwise performing a single token exchange
/// (`cpt-cf-oagw-algo-plugin-token-acquire`, steps 1-12).
// @cpt-algo:cpt-cf-oagw-algo-plugin-token-acquire:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-token-cache:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-no-retry-on-401:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-12
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-13
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-14
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-15
#[allow(clippy::too_many_arguments)]
pub(crate) async fn acquire_token(
    config: &Value,
    method: ClientAuthMethod,
    ctx: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
    cache: &TokenCache,
    ttl_ceiling_secs: u64,
    proxy_timeout_secs: u32,
) -> Result<SecretString, TokenAcquireError> {
    let parsed = parse_config(config)?;

    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-06
    let cache_key = build_cache_key(ctx, method, config);
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-06
    if let Some(token) = cache.get(&cache_key) {
        return Ok(token);
    }

    let client_id = resolve_secret(&parsed.client_id_ref, ctx, credstore)
        .await
        .map_err(TokenAcquireError::Credential)?;
    let client_secret = resolve_secret(&parsed.client_secret_ref, ctx, credstore)
        .await
        .map_err(TokenAcquireError::Credential)?;

    let token_endpoint = parsed
        .token_endpoint
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(|_| TokenAcquireError::InvalidEndpoint)?;
    let issuer_url = parsed
        .issuer_url
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(|_| TokenAcquireError::InvalidEndpoint)?;

    let oauth_config = OAuthClientConfig {
        token_endpoint,
        issuer_url,
        client_id: client_id.expose().to_owned(),
        client_secret: SecretString::new(client_secret.expose().to_owned()),
        scopes: parsed
            .scopes
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        auth_method: method,
        http_config: Some(bounded_http_config(proxy_timeout_secs)),
        ..Default::default()
    };

    let fetched = fetch_token(oauth_config)
        .await
        .map_err(|err: TokenError| TokenAcquireError::Fetch(err.to_string()))?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-15
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-14
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-13
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-12

    match effective_ttl_secs(ttl_ceiling_secs, fetched.expires_in.as_secs()) {
        Some(ttl_secs) => {
            cache.put(
                cache_key,
                fetched.bearer.clone(),
                tokio::time::Duration::from_secs(ttl_secs),
            );
        }
        // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-02
        None => {
            // `expires_in` at or below the safety margin: use for this
            // request only, write no cache entry -- `Absent` stays
            // `Absent` (the sibling `Absent -> Absent` transition is the
            // `fetch_token(...)?` failure path just above, which returns
            // before ever reaching this `match`).
        } // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-02
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-20
    // Retry-on-401 is explicitly deferred by
    // `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` and MUST NOT
    // be added here: this function neither observes the upstream's status
    // nor evicts a cache entry in response to one.
    Ok(fetched.bearer)
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-20
}
// (steps 16-20 -- ttl computation, the margin check, storing, and the
// no-retry-on-401 contract -- are covered by `effective_ttl_secs` and
// `super::token_cache::TokenCache::put`, exercised above and unit-tested
// directly in `super::token_cache`.)

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    fn creds() -> MockCredStoreClient {
        MockCredStoreClient::with_secrets(vec![
            ("client-id".to_owned(), "the-client-id".to_owned()),
            ("client-secret".to_owned(), "the-client-secret".to_owned()),
        ])
    }

    fn token_body(token: &str, expires_in: u64) -> String {
        format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
    }

    #[test]
    fn both_endpoint_keys_present_is_rejected_before_any_lookup() {
        let config = json!({
            "token_endpoint": "https://a.example.com/token",
            "issuer_url": "https://b.example.com",
            "client_id_ref": "cred://x",
            "client_secret_ref": "cred://y",
        });
        assert_eq!(
            parse_config(&config).unwrap_err(),
            TokenAcquireError::ExclusiveOrViolation
        );
    }

    #[test]
    fn neither_endpoint_key_is_rejected() {
        let config = json!({"client_id_ref": "cred://x", "client_secret_ref": "cred://y"});
        assert_eq!(
            parse_config(&config).unwrap_err(),
            TokenAcquireError::ExclusiveOrViolation
        );
    }

    #[test]
    fn missing_client_id_ref_is_rejected() {
        let config = json!({"token_endpoint": "https://a.example.com/token", "client_secret_ref": "cred://y"});
        assert_eq!(
            parse_config(&config).unwrap_err(),
            TokenAcquireError::MissingKey("client_id_ref")
        );
    }

    #[tokio::test]
    async fn exclusive_or_violation_makes_no_credential_or_network_call() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let config = json!({
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let err = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &ctx(),
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap_err();
        assert_eq!(err, TokenAcquireError::ExclusiveOrViolation);
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn form_variant_performs_exchange_with_credentials_in_the_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .body_includes("client_id=the-client-id");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok-form", 3600));
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let token = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &ctx(),
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        assert_eq!(token.expose(), "tok-form");
        mock.assert();
    }

    #[tokio::test]
    async fn basic_variant_performs_exchange_with_credentials_in_the_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .header_exists("authorization");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok-basic", 3600));
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let token = acquire_token(
            &config,
            ClientAuthMethod::Basic,
            &ctx(),
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        assert_eq!(token.expose(), "tok-basic");
        mock.assert();
    }

    #[tokio::test]
    async fn issuer_url_resolves_the_endpoint_via_discovery() {
        let server = MockServer::start();
        let token_ep = format!("http://{}/oauth/token", server.address());
        server.mock(|when, then| {
            when.method(GET).path("/.well-known/openid-configuration");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(r#"{{"token_endpoint":"{token_ep}"}}"#));
        });
        server.mock(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok-discovered", 3600));
        });
        let config = json!({
            "issuer_url": format!("http://{}", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let token = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &ctx(),
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        assert_eq!(token.expose(), "tok-discovered");
    }

    #[tokio::test]
    async fn a_second_request_for_the_same_tuple_is_served_from_cache() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok-cached", 3600));
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let same_ctx = ctx();
        let first = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        let second = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        assert_eq!(first.expose(), second.expose());
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn differing_scopes_cause_a_separate_exchange() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok", 3600));
        });
        let base = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let mut with_scope = base.clone();
        with_scope["scopes"] = json!("read");
        let cache = TokenCache::new(10);
        let same_ctx = ctx();
        acquire_token(
            &base,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        acquire_token(
            &with_scope,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn form_and_basic_variants_never_share_a_cache_entry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok", 3600));
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let same_ctx = ctx();
        acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        acquire_token(
            &config,
            ClientAuthMethod::Basic,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn expires_in_at_or_below_margin_is_used_once_and_not_cached() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_body("tok-short", 10));
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let same_ctx = ctx();
        let cache_key = build_cache_key(&same_ctx, ClientAuthMethod::Form, &config);
        acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        assert!(cache.get(&cache_key).is_none());
        // A second request repeats the exchange rather than serving a
        // never-cached entry.
        acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap();
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn a_failed_exchange_is_not_cached_and_the_next_request_retries() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(500).body("internal server error");
        });
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let same_ctx = ctx();
        let first = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await;
        assert!(first.is_err());
        let second = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &same_ctx,
            &creds(),
            &cache,
            300,
            5,
        )
        .await;
        assert!(second.is_err());
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn a_missing_client_secret_reference_maps_to_a_credential_error() {
        let server = MockServer::start();
        let config = json!({
            "token_endpoint": format!("http://{}/token", server.address()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://absent",
        });
        let cache = TokenCache::new(10);
        let err = acquire_token(
            &config,
            ClientAuthMethod::Form,
            &ctx(),
            &creds(),
            &cache,
            300,
            5,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TokenAcquireError::Credential(_)));
    }

    #[tokio::test]
    async fn an_unresponsive_identity_provider_fails_within_the_proxy_timeout_bound() {
        // A listener that accepts but never responds: the client must give
        // up at `proxy_timeout_secs` rather than hanging.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept and hold the connection open without ever writing a
            // response, for the lifetime of the test process.
            let _ = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        let config = json!({
            "token_endpoint": format!("http://{addr}/token"),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            acquire_token(
                &config,
                ClientAuthMethod::Form,
                &ctx(),
                &creds(),
                &cache,
                300,
                1,
            ),
        )
        .await
        .expect(
            "acquire_token must itself respect proxy_timeout_secs, not hang past the outer guard",
        );
        assert!(result.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(9),
            "must fail near the 1s proxy_timeout_secs bound, not the 10s outer guard"
        );
    }

    #[test]
    fn safe_detail_never_contains_a_reference_value_or_secret() {
        let err = TokenAcquireError::Fetch("OAuth2 token HTTP 401 Unauthorized".to_owned());
        let detail = err.safe_detail();
        assert!(!detail.contains("401 Unauthorized"));
        assert!(!detail.to_lowercase().contains("secret"));
    }
}
