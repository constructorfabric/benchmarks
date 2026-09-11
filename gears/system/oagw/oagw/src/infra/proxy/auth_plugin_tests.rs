//! The built-in auth plugins as the data plane runs them (PRD §5.2, ADR 0008).
//!
//! Each test drives a real [`DataPlaneService`] against a live `httpmock`
//! upstream with the built-in auth registry over a `MockCredStoreClient`, so the
//! credential injection is observed on the wire — and the assertion is that the
//! injected secret reaches the upstream and never comes back to the client.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderMap, Method};
use httpmock::prelude::*;
use serde_json::json;

use crate::config::OagwConfig;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    HeadersConfig, HttpMatch, HttpMethod, MatchConfig, PassthroughMode, PathSuffixMode,
    PluginsConfig, Route, SharingMode, Upstream,
};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::service::{DataPlaneService, ProxyBody, ProxyCall};
use crate::infra::proxy::service_tests::{plain_upstream, setup_with};

const TENANT: &str = "00000000-0000-0000-0000-000000000001";
const SECRET: &str = "sk-live-oagw-key";

/// An upstream whose `auth` block is supplied by the test, plus a GET route.
async fn auth_fixture(
    server: &MockServer,
    plugin_type: &str,
    config: BTreeMap<String, serde_json::Value>,
) -> crate::domain::services::management::ControlPlaneService {
    let upstream = Upstream {
        auth: Some(crate::domain::model::AuthConfig {
            plugin_type: Some(plugin_type.to_owned()),
            sharing: SharingMode::Private,
            config,
        }),
        headers: HeadersConfig {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::All,
                ..crate::domain::model::RequestHeaderRules::default()
            },
            ..HeadersConfig::default()
        },
        ..plain_upstream()
    };
    let route = Route {
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/api".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..Route::default()
    };
    let (cp, _fixture) = setup_with(server, upstream, route, Some("backend")).await;
    cp
}

/// A data plane whose auth plugins are the built-ins over the given credential
/// store.
fn data_plane(cp: crate::domain::services::management::ControlPlaneService) -> DataPlaneService {
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            vec![("openai-key".to_owned(), SECRET.to_owned())],
        ));
    DataPlaneService::new(
        Arc::new(cp),
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
    )
    .expect("a buildable data plane")
    .with_registries(
        AuthPluginRegistry::with_builtins(&credstore, &OagwConfig::default()),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

async fn get(
    dp: &DataPlaneService,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
) -> http::Response<ProxyBody> {
    let mut header_map = HeaderMap::new();
    for (name, value) in headers {
        header_map.insert(
            http::HeaderName::from_bytes(name.as_bytes()).expect("static header name"),
            http::HeaderValue::from_str(value).expect("static header value"),
        );
    }
    dp.proxy(ProxyCall {
        tenant_id: TENANT.to_owned(),
        user_id: None,
        client_ip: None,
        method: Method::GET,
        path: path.to_owned(),
        query: query.to_owned(),
        headers: header_map,
        body: bytes::Bytes::new(),
        upgrade: None,
    })
    .await
}

/// A bound `plugins.items[]` entry with the given configuration.
fn bound(
    plugin_ref: &str,
    config: BTreeMap<String, serde_json::Value>,
) -> crate::domain::model::PluginBinding {
    crate::domain::model::PluginBinding::Bound {
        plugin_ref: plugin_ref.to_owned(),
        plugin_uuid: None,
        config: Some(config),
    }
}

#[tokio::test]
async fn noop_auth_leaves_the_request_unmodified() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header_missing("x-api-key");
        then.status(200).body("ok");
    });

    let cp = auth_fixture(&server, gts::AUTH_NOOP, BTreeMap::new()).await;
    let response = get(&data_plane(cp), "/backend/api", "", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert();
}

#[tokio::test]
async fn the_api_key_is_injected_as_a_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header("x-api-key", SECRET);
        then.status(200).body("ok");
    });

    let mut config = BTreeMap::new();
    config.insert("key_ref".to_owned(), json!("cred://openai-key"));
    config.insert("header".to_owned(), json!("x-api-key"));
    let cp = auth_fixture(&server, gts::AUTH_APIKEY, config).await;

    let mut response = get(&data_plane(cp), "/backend/api", "", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert();
    // The credential value never reaches the client: the response is the
    // upstream's own body and headers, none of which carry the secret.
    let body = http_body_util::BodyExt::collect(response.body_mut())
        .await
        .expect("a collectable body")
        .to_bytes();
    assert_eq!(body.as_ref(), b"ok");
    assert!(
        response
            .headers()
            .iter()
            .all(|(_, value)| value.as_bytes() != SECRET.as_bytes()),
        "the api key must not be echoed back to the client"
    );
}

#[tokio::test]
async fn the_api_key_can_be_injected_into_the_query_string() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").query_param("api_key", SECRET);
        then.status(200).body("ok");
    });

    let mut config = BTreeMap::new();
    config.insert("key_ref".to_owned(), json!("openai-key"));
    config.insert("in".to_owned(), json!("query"));
    config.insert("query".to_owned(), json!("api_key"));
    let cp = auth_fixture(&server, gts::AUTH_APIKEY, config).await;

    // The allowlist is checked before the auth phase runs, so the injected
    // parameter needs no entry of its own.
    let response = get(&data_plane(cp), "/backend/api", "", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert();
}

#[tokio::test]
async fn a_missing_secret_is_an_internal_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("unreachable");
    });

    let mut config = BTreeMap::new();
    config.insert("key_ref".to_owned(), json!("cred://absent-key"));
    let cp = auth_fixture(&server, gts::AUTH_APIKEY, config).await;

    let mut response = get(&data_plane(cp), "/backend/api", "", &[]).await;
    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = http_body_util::BodyExt::collect(response.body_mut())
        .await
        .expect("a collectable body")
        .to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(
        problem["type"],
        format!("gts.cf.core.errors.err.v1~{}", gts::ERR_SECRET_NOT_FOUND)
    );
    assert_eq!(problem["title"], "Secret Not Found");
}

/// An upstream binding the `request_id` transform is used here because the
/// `apikey` guard-free path above already covers the auth phase; this test
/// pins the plugin-configuration plumbing through `plugins.items[]`.
#[tokio::test]
async fn bound_plugin_configuration_reaches_the_implementation() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header_exists("x-request-id");
        then.status(200).body("ok");
    });

    let upstream = Upstream {
        headers: HeadersConfig {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::All,
                ..crate::domain::model::RequestHeaderRules::default()
            },
            ..HeadersConfig::default()
        },
        plugins: PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![bound(gts::TRANSFORM_REQUEST_ID, BTreeMap::new())],
        },
        ..plain_upstream()
    };
    let route = Route {
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/api".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..Route::default()
    };
    let (cp, _fixture) = setup_with(&server, upstream, route, Some("backend")).await;
    let response = get(&data_plane(cp), "/backend/api", "", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert();
}
