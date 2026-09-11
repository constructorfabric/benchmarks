#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the OAuth2 client-credentials plugins (ADR-0008).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::routing::post;
use bytes::Bytes;
use uuid::Uuid;

use super::{ClientAuthMethod, OAuth2ClientCredAuthPlugin};
use crate::domain::gts_helpers::{AUTH_OAUTH2_CC, AUTH_OAUTH2_CC_BASIC};
use crate::domain::plugin::{AuthPlugin, PluginError};
use crate::infra::credentials::{CredStoreResolver, hash_config};

fn request(tenant: Uuid) -> crate::domain::plugin::ProxyRequest {
    crate::domain::plugin::ProxyRequest {
        method: axum::http::Method::GET,
        path: "/things".to_string(),
        query: String::new(),
        headers: axum::http::HeaderMap::new(),
        body: Bytes::new(),
        tenant_id: tenant,
        security: None,
    }
}

fn store_resolver() -> crate::infra::credentials::SecretResolver {
    let store = credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
        ("gateway-client-id".to_string(), "the-client".to_string()),
        ("gateway-client-secret".to_string(), "the-secret".to_string()),
    ]);
    Arc::new(CredStoreResolver::new(Arc::new(store)))
}

/// A local token endpoint that records the shape of the requests it receives.
struct TokenEndpoint {
    base: String,
    grants: Arc<std::sync::Mutex<Vec<String>>>,
    authorizations: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn serve_token_endpoint() -> TokenEndpoint {
    let grants = Arc::new(std::sync::Mutex::new(Vec::new()));
    let authorizations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let grants_in = Arc::clone(&grants);
    let auth_in = Arc::clone(&authorizations);
    let app = Router::new().route(
        "/token",
        post(move |headers: axum::http::HeaderMap, body: Body| {
            let grants = Arc::clone(&grants_in);
            let authorizations = Arc::clone(&auth_in);
            async move {
                let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
                grants.lock().unwrap().push(String::from_utf8_lossy(&bytes).to_string());
                authorizations
                    .lock()
                    .unwrap()
                    .push(headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string());
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "access_token": "issued-token-1",
                        "token_type": "Bearer",
                        "expires_in": 1800,
                    })),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TokenEndpoint {
        base: format!("http://127.0.0.1:{port}"),
        grants,
        authorizations,
    }
}

use axum::response::IntoResponse;

#[tokio::test]
async fn it_exchanges_the_configured_credentials_for_a_bearer_token() {
    let endpoint = serve_token_endpoint().await;
    let plugin = OAuth2ClientCredAuthPlugin::form().with_resolver(store_resolver());
    let mut request = request(Uuid::now_v7());
    plugin
        .authenticate(
            &mut request,
            &serde_json::json!({
                "token_endpoint": format!("{}/token", endpoint.base),
                "client_id_ref": "gateway-client-id",
                "client_secret_ref": "gateway-client-secret",
            }),
        )
        .await
        .unwrap();
    assert_eq!(request.header("authorization").as_deref(), Some("Bearer issued-token-1"));
    assert!(!endpoint.authorizations.lock().unwrap().is_empty(), "the token endpoint must be called");
    let grant = endpoint.grants.lock().unwrap().first().cloned().unwrap_or_default();
    assert!(grant.contains("grant_type=client_credentials"), "{grant}");
    assert!(grant.contains("client_id=the-client"), "{grant}");
    assert!(
        grant.contains("client_secret=the-secret"),
        "the form variant carries the credentials in the body: {grant}"
    );
    assert!(
        endpoint.authorizations.lock().unwrap().iter().all(String::is_empty),
        "the form variant must not use the basic header"
    );
}

#[tokio::test]
async fn the_second_call_is_served_from_the_cache() {
    let endpoint = serve_token_endpoint().await;
    let plugin = OAuth2ClientCredAuthPlugin::form().with_resolver(store_resolver());
    let config = serde_json::json!({
        "token_endpoint": format!("{}/token", endpoint.base),
        "client_id_ref": "gateway-client-id",
        "client_secret_ref": "gateway-client-secret",
    });
    let mut first = request(Uuid::now_v7());
    plugin.authenticate(&mut first, &config).await.unwrap();
    let mut second = request(first.tenant_id);
    plugin.authenticate(&mut second, &config).await.unwrap();
    assert_eq!(second.header("authorization"), first.header("authorization"));
    assert_eq!(endpoint.grants.lock().unwrap().len(), 1, "only one token exchange may happen");
}

#[tokio::test]
async fn the_client_secret_travels_in_the_basic_header_for_the_basic_variant() {
    let endpoint = serve_token_endpoint().await;
    let plugin = OAuth2ClientCredAuthPlugin::basic().with_resolver(store_resolver());
    let mut request = request(Uuid::now_v7());
    plugin
        .authenticate(
            &mut request,
            &serde_json::json!({
                "token_endpoint": format!("{}/token", endpoint.base),
                "client_id_ref": "gateway-client-id",
                "client_secret_ref": "gateway-client-secret",
            }),
        )
        .await
        .unwrap();
    let grant = endpoint.grants.lock().unwrap().first().cloned().unwrap_or_default();
    assert!(!grant.contains("client_id="), "basic auth must not use the form body: {grant}");
    assert!(!grant.contains("the-secret"), "the secret must not travel in the body either: {grant}");
    assert_eq!(
        endpoint.authorizations.lock().unwrap().first().map(String::as_str),
        Some("Basic dGhlLWNsaWVudDp0aGUtc2VjcmV0"),
        "the basic variant encodes both credentials in the Authorization header"
    );
    assert_eq!(request.header("authorization").as_deref(), Some("Bearer issued-token-1"));
}

#[tokio::test]
async fn a_missing_secret_is_reported_and_not_echoed() {
    let plugin = OAuth2ClientCredAuthPlugin::form().with_resolver(Arc::new(
        crate::infra::credentials::MissingResolver,
    ));
    let mut request = request(Uuid::now_v7());
    let error = plugin
        .authenticate(
            &mut request,
            &serde_json::json!({
                "token_endpoint": "http://127.0.0.1:9/token",
                "client_id_ref": "cred://absent",
                "client_secret_ref": "cred://absent",
            }),
        )
        .await
        .unwrap_err();
    match error {
        PluginError::SecretNotFound(detail) => assert!(!detail.contains("the-secret")),
        other => panic!("expected SecretNotFound, got {other:?}"),
    }
    assert!(
        request.header("authorization").is_none(),
        "no credential may be injected when the exchange fails"
    );
}

#[tokio::test]
async fn an_incomplete_configuration_is_rejected_before_any_call() {
    let plugin = OAuth2ClientCredAuthPlugin::form().with_resolver(store_resolver());
    for config in [
        serde_json::json!({}),
        serde_json::json!({"token_endpoint": "http://127.0.0.1:9/token"}),
        serde_json::json!({"token_endpoint": "http://127.0.0.1:9/token", "client_id_ref": "cred://a"}),
    ] {
        let mut request = request(Uuid::now_v7());
        let error = plugin.authenticate(&mut request, &config).await.unwrap_err();
        assert!(matches!(error, PluginError::Config(_)), "{config} -> {error:?}");
        assert!(request.header("authorization").is_none());
    }
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_an_auth_failure() {
    let plugin = OAuth2ClientCredAuthPlugin::form().with_resolver(store_resolver());
    let mut request = request(Uuid::now_v7());
    let error = plugin
        .authenticate(
            &mut request,
            &serde_json::json!({
                "token_endpoint": "http://127.0.0.1:9/token",
                "client_id_ref": "gateway-client-id",
                "client_secret_ref": "gateway-client-secret",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, PluginError::AuthFailed(_)),
        "a refused connection must surface as an auth failure, not a panic: {error:?}"
    );
}

#[test]
fn the_two_variants_advertise_their_own_identifiers() {
    assert_eq!(OAuth2ClientCredAuthPlugin::form().id(), AUTH_OAUTH2_CC);
    assert_eq!(OAuth2ClientCredAuthPlugin::basic().id(), AUTH_OAUTH2_CC_BASIC);
    assert_eq!(OAuth2ClientCredAuthPlugin::form().method, ClientAuthMethod::Form);
    assert_eq!(OAuth2ClientCredAuthPlugin::basic().method, ClientAuthMethod::Basic);
}

#[test]
fn the_cache_key_is_derived_from_the_config_so_distinct_configs_do_not_share_tokens() {
    let first = serde_json::json!({"token_endpoint": "https://a/token", "client_id_ref": "cred://one"});
    let second = serde_json::json!({"client_id_ref": "cred://one", "token_endpoint": "https://a/token"});
    let third = serde_json::json!({"token_endpoint": "https://a/token", "client_id_ref": "cred://two"});
    assert_eq!(hash_config(&first), hash_config(&second), "key order must not matter");
    assert_ne!(hash_config(&first), hash_config(&third), "a different client is a different key");
    assert_eq!(hash_config(&serde_json::json!({"a": null})), String::new(), "nulls are ignored");
}
