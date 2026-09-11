//! The `OAuth2` client-credentials auth plugins (`Form` and `Basic` variants)
//! (`cpt-cf-oagw-algo-oauth2-token-acquisition`, `cpt-cf-oagw-algo-token-cache-lookup`,
//! `cpt-cf-oagw-dod-oauth2-client-cred-auth`, `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`).
//!
//! Both variants share this module and differ only in
//! [`toolkit_auth::oauth2::ClientAuthMethod`]; caching, credential
//! resolution, and token acquisition are identical.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue};
use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::binding::{config_hash, config_str, parse_cred_ref, resolve_secret};
use super::token_cache::TokenCache;
use crate::error::OagwError;

/// The token-expiry safety margin (`cpt-cf-oagw-algo-oauth2-token-acquisition`):
/// a token reporting an expiry at or below this margin is never cached.
const SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Fixed knobs threaded from `OagwConfig`/`config.rs` and the gear's
/// outbound-HTTP posture, bundled to keep [`inject_client_credentials`]'s
/// signature manageable.
pub struct Oauth2Params {
    /// `OagwConfig::token_cache_ttl_secs`, the configured cache-lifetime
    /// ceiling.
    pub token_cache_ttl_ceiling: Duration,
    /// `OagwConfig::proxy_timeout_secs`, bounding both the credential-store
    /// lookups and the token-endpoint exchange
    /// (`cpt-cf-oagw-dod-credential-isolation`).
    pub proxy_timeout: Duration,
    /// Test-only override for the internal token-endpoint HTTP client
    /// configuration (e.g. to allow plaintext `httpmock` servers). `None`
    /// in production, which uses `toolkit_http::HttpClientConfig::token_endpoint()`.
    pub http_config_override: Option<toolkit_http::HttpClientConfig>,
}

/// Builds the token-cache key: subject tenant, subject, client-auth variant
/// tag, and a deterministic configuration hash
/// (`cpt-cf-oagw-algo-token-cache-lookup`). Each component isolates a
/// distinct boundary — see the module's ADR for the security rationale.
#[must_use]
pub fn build_cache_key(
    tenant_id: Uuid,
    subject_id: Uuid,
    auth_method: ClientAuthMethod,
    config: Option<&serde_json::Value>,
) -> String {
    let variant = match auth_method {
        ClientAuthMethod::Form => "form",
        ClientAuthMethod::Basic => "basic",
    };
    format!(
        "{tenant_id}:{subject_id}:{variant}:{:x}",
        config_hash(config)
    )
}

/// Acquires (or reuses a cached) `OAuth2` client-credentials access token and
/// injects it as `Authorization: Bearer <token>`
/// (`cpt-cf-oagw-algo-oauth2-token-acquisition`).
///
/// `config`'s keys: `token_endpoint` XOR `issuer_url` (required, mutually
/// exclusive), `client_id_ref`, `client_secret_ref` (both required `cred://`
/// references), and optional space-separated `scopes`.
///
/// # Errors
///
/// Returns [`OagwError::plugin_not_found`] (`503`) when the binding names
/// both or neither of `token_endpoint`/`issuer_url`, or either credential
/// reference is absent or inline (`cpt-cf-oagw-dod-credential-isolation`).
/// Returns [`OagwError::secret_not_found`]/[`OagwError::authentication_failed`]
/// per [`resolve_secret`]'s mapping, and [`OagwError::authentication_failed`]
/// when token acquisition itself fails or exceeds `params.proxy_timeout` —
/// a failed acquisition is never cached (`cpt-cf-oagw-dod-token-cache`).
// @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p2:inst-oauth2-inject-fn-01
#[allow(clippy::too_many_arguments)]
pub async fn inject_client_credentials(
    auth_method: ClientAuthMethod,
    config: Option<&serde_json::Value>,
    credstore: &dyn CredStoreClientV1,
    security_context: &SecurityContext,
    tenant_id: Uuid,
    subject_id: Uuid,
    token_cache: &TokenCache,
    params: &Oauth2Params,
    headers: &mut HeaderMap,
) -> Result<(), OagwError> {
    let token_endpoint = config_str(config, "token_endpoint");
    let issuer_url = config_str(config, "issuer_url");
    if token_endpoint.is_some() == issuer_url.is_some() {
        // Both present, or both absent: either way the binding is
        // structurally invalid (`cpt-cf-oagw-dod-credential-isolation`).
        return Err(OagwError::plugin_not_found(
            "oauth2 binding must name exactly one of token_endpoint or issuer_url",
        ));
    }

    let cache_key = build_cache_key(tenant_id, subject_id, auth_method, config);
    if let Some(bearer) = token_cache.get(&cache_key) {
        return inject_bearer(headers, bearer.expose());
    }

    let client_id_ref =
        parse_cred_ref(config.and_then(|c| c.get("client_id_ref"))).ok_or_else(|| {
            OagwError::plugin_not_found("oauth2 binding requires a 'cred://' client_id_ref")
        })?;
    let client_secret_ref = parse_cred_ref(config.and_then(|c| c.get("client_secret_ref")))
        .ok_or_else(|| {
            OagwError::plugin_not_found("oauth2 binding requires a 'cred://' client_secret_ref")
        })?;

    let client_id = resolve_secret(
        credstore,
        security_context,
        &client_id_ref,
        params.proxy_timeout,
    )
    .await?;
    let client_secret = resolve_secret(
        credstore,
        security_context,
        &client_secret_ref,
        params.proxy_timeout,
    )
    .await?;

    let scopes = config_str(config, "scopes")
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut oauth_config = OAuthClientConfig {
        client_id: client_id.expose().to_owned(),
        client_secret: toolkit_auth::oauth2::SecretString::new(client_secret.expose().to_owned()),
        scopes,
        auth_method,
        http_config: params.http_config_override.clone(),
        ..OAuthClientConfig::default()
    };
    if let Some(endpoint) = token_endpoint {
        oauth_config.token_endpoint = url::Url::parse(endpoint).ok();
    }
    if let Some(issuer) = issuer_url {
        oauth_config.issuer_url = url::Url::parse(issuer).ok();
    }

    let fetched = tokio::time::timeout(params.proxy_timeout, fetch_token(oauth_config))
        .await
        .map_err(|_| {
            OagwError::authentication_failed(
                "token endpoint did not respond within the configured proxy timeout",
            )
        })?
        .map_err(|error| {
            OagwError::authentication_failed(format!("token acquisition failed: {error}"))
        })?;

    let ttl = params
        .token_cache_ttl_ceiling
        .min(fetched.expires_in.saturating_sub(SAFETY_MARGIN));
    if !ttl.is_zero() {
        token_cache.put(&cache_key, fetched.bearer.clone(), ttl);
    }

    inject_bearer(headers, fetched.bearer.expose())
}
// @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p2:inst-oauth2-inject-fn-01

fn inject_bearer(headers: &mut HeaderMap, token: &str) -> Result<(), OagwError> {
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        OagwError::authentication_failed("resolved access token is not a valid header value")
    })?;
    headers.insert(AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Oauth2Params, build_cache_key, inject_client_credentials};
    use crate::domain::plugin::token_cache::TokenCache;
    use axum::http::HeaderMap;
    use credstore_sdk::test_util::MockCredStoreClient;
    use httpmock::MockServer;
    use serde_json::json;
    use std::time::Duration;
    use toolkit_auth::oauth2::ClientAuthMethod;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::anonymous()
    }

    fn credstore() -> MockCredStoreClient {
        MockCredStoreClient::with_secrets(vec![
            ("client-id".to_owned(), "the-client-id".to_owned()),
            ("client-secret".to_owned(), "the-client-secret".to_owned()),
        ])
    }

    fn params() -> Oauth2Params {
        Oauth2Params {
            token_cache_ttl_ceiling: Duration::from_mins(5),
            proxy_timeout: Duration::from_secs(5),
            http_config_override: Some(toolkit_http::HttpClientConfig::for_testing()),
        }
    }

    fn token_json(token: &str, expires_in: u64) -> String {
        format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
    }

    // @cpt-begin:cpt-cf-oagw-dod-oauth2-client-cred-auth:p2:inst-oauth2-fetch-inject-test-01
    #[tokio::test]
    async fn a_fetched_token_is_injected_as_a_bearer_header() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_json("tok-1", 3600));
        });

        let config = json!({
            "token_endpoint": format!("http://{}:{}/token", server.host(), server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let mut headers = HeaderMap::new();

        inject_client_credentials(
            ClientAuthMethod::Form,
            Some(&config),
            &credstore(),
            &ctx(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            &cache,
            &params(),
            &mut headers,
        )
        .await
        .expect("must inject");

        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer tok-1")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-oauth2-client-cred-auth:p2:inst-oauth2-fetch-inject-test-01

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-cache-reuse-test-01
    #[tokio::test]
    async fn a_second_request_reuses_the_cached_token_with_exactly_one_token_call() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_json("tok-cached", 3600));
        });

        let config = json!({
            "token_endpoint": format!("http://{}:{}/token", server.host(), server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let tenant_id = Uuid::new_v4();
        let subject_id = Uuid::new_v4();

        for _ in 0..2 {
            let mut headers = HeaderMap::new();
            inject_client_credentials(
                ClientAuthMethod::Form,
                Some(&config),
                &credstore(),
                &ctx(),
                tenant_id,
                subject_id,
                &cache,
                &params(),
                &mut headers,
            )
            .await
            .expect("must inject");
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer tok-cached")
            );
        }

        assert_eq!(mock.calls(), 1);
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-cache-reuse-test-01

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-short-lived-test-01
    #[tokio::test]
    async fn a_token_shorter_than_the_safety_margin_is_never_cached() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_json("tok-short", 20));
        });

        let config = json!({
            "token_endpoint": format!("http://{}:{}/token", server.host(), server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let tenant_id = Uuid::new_v4();
        let subject_id = Uuid::new_v4();

        for _ in 0..2 {
            let mut headers = HeaderMap::new();
            inject_client_credentials(
                ClientAuthMethod::Form,
                Some(&config),
                &credstore(),
                &ctx(),
                tenant_id,
                subject_id,
                &cache,
                &params(),
                &mut headers,
            )
            .await
            .expect("must inject");
        }

        assert_eq!(mock.calls(), 2);
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-short-lived-test-01

    // @cpt-begin:cpt-cf-oagw-dod-failure-mapping:p2:inst-oauth2-failure-not-cached-test-01
    #[tokio::test]
    async fn a_failed_fetch_is_never_cached_and_the_next_request_retries() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(500).body("nope");
        });

        let config = json!({
            "token_endpoint": format!("http://{}:{}/token", server.host(), server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let tenant_id = Uuid::new_v4();
        let subject_id = Uuid::new_v4();

        for _ in 0..2 {
            let mut headers = HeaderMap::new();
            let error = inject_client_credentials(
                ClientAuthMethod::Form,
                Some(&config),
                &credstore(),
                &ctx(),
                tenant_id,
                subject_id,
                &cache,
                &params(),
                &mut headers,
            )
            .await
            .expect_err("failed fetch must error");
            assert_eq!(error.status(), axum::http::StatusCode::UNAUTHORIZED);
        }

        assert_eq!(mock.calls(), 2);
    }
    // @cpt-end:cpt-cf-oagw-dod-failure-mapping:p2:inst-oauth2-failure-not-cached-test-01

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-tenant-isolation-test-01
    #[tokio::test]
    async fn two_subjects_in_different_tenants_each_cause_their_own_token_call() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/token");
            then.status(200)
                .header("content-type", "application/json")
                .body(token_json("tok-multi", 3600));
        });

        let config = json!({
            "token_endpoint": format!("http://{}:{}/token", server.host(), server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);

        for _ in 0..2 {
            let mut headers = HeaderMap::new();
            inject_client_credentials(
                ClientAuthMethod::Form,
                Some(&config),
                &credstore(),
                &ctx(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                &cache,
                &params(),
                &mut headers,
            )
            .await
            .expect("must inject");
        }

        assert_eq!(mock.calls(), 2);
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-oauth2-tenant-isolation-test-01

    // @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-oauth2-both-endpoints-test-01
    #[tokio::test]
    async fn naming_both_a_token_endpoint_and_an_issuer_is_rejected_with_no_call() {
        let config = json!({
            "token_endpoint": "https://example.com/token",
            "issuer_url": "https://issuer.example.com",
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        });
        let cache = TokenCache::new(10);
        let mut headers = HeaderMap::new();

        let error = inject_client_credentials(
            ClientAuthMethod::Form,
            Some(&config),
            &credstore(),
            &ctx(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            &cache,
            &params(),
            &mut headers,
        )
        .await
        .expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    // @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-oauth2-both-endpoints-test-01

    #[test]
    fn cache_key_differs_by_client_auth_variant() {
        let tenant_id = Uuid::new_v4();
        let subject_id = Uuid::new_v4();
        let config = json!({"token_endpoint": "https://a.example.com"});
        let form_key =
            build_cache_key(tenant_id, subject_id, ClientAuthMethod::Form, Some(&config));
        let basic_key = build_cache_key(
            tenant_id,
            subject_id,
            ClientAuthMethod::Basic,
            Some(&config),
        );
        assert_ne!(form_key, basic_key);
    }
}
