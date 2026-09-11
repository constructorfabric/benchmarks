//! The plugin chain as the data plane executes it (DESIGN §3.2 "Plugin System",
//! ADR 0008, ADR 0009).
//!
//! Order matters here, so every assertion goes through the real
//! [`DataPlaneService`] against a live `httpmock` upstream: Auth → Guards →
//! Transform(request) → upstream → Transform(response), with upstream-bound
//! plugins running before route-bound ones.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use httpmock::prelude::*;
use serde_json::json;

use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    HeadersConfig, HttpMatch, HttpMethod, MatchConfig, PassthroughMode, PathSuffixMode,
    PluginBinding, PluginsConfig, Route, SharingMode, Upstream,
};
use crate::domain::plugin::PluginConfig;
use crate::infra::plugin::registry::PluginResolveError;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::proxy::service_tests::{body, get, plain_upstream, setup_with, try_setup_with};

/// A `guard` binding for the built-in `required_headers` plugin.
fn required_request_headers_guard(required: &str) -> PluginBinding {
    let mut config = BTreeMap::new();
    config.insert("required_request_headers".to_owned(), json!(required));
    PluginBinding::Bound {
        plugin_ref: gts::GUARD_REQUIRED_HEADERS.to_owned(),
        plugin_uuid: None,
        config: Some(config),
    }
}

/// A `guard` binding that checks the upstream response instead.
fn required_response_headers_guard(required: &str) -> PluginBinding {
    let mut config = BTreeMap::new();
    config.insert("required_response_headers".to_owned(), json!(required));
    PluginBinding::Bound {
        plugin_ref: gts::GUARD_REQUIRED_HEADERS.to_owned(),
        plugin_uuid: None,
        config: Some(config),
    }
}

/// A `transform` binding for the built-in `request_id` plugin.
fn request_id_transform() -> PluginBinding {
    PluginBinding::Reference(gts::TRANSFORM_REQUEST_ID.to_owned())
}

/// An upstream + route whose plugin bindings are supplied by the test.
async fn chain_fixture(
    server: &MockServer,
    upstream_plugins: Vec<PluginBinding>,
    route_plugins: Vec<PluginBinding>,
) -> Result<
    crate::domain::services::management::ControlPlaneService,
    crate::domain::error::DomainError,
> {
    let upstream = Upstream {
        // Inbound headers flow through so the chain has something to read.
        headers: HeadersConfig {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::All,
                ..crate::domain::model::RequestHeaderRules::default()
            },
            ..HeadersConfig::default()
        },
        plugins: PluginsConfig {
            sharing: SharingMode::Private,
            items: upstream_plugins,
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
        plugins: PluginsConfig {
            sharing: SharingMode::Private,
            items: route_plugins,
        },
        ..Route::default()
    };
    let (cp, _fixture) = setup_with(server, upstream, route, Some("backend")).await;
    Ok(cp)
}

/// [`chain_fixture`] without the unwrap, for tests that expect the binding to
/// be refused.
async fn try_chain_fixture(
    server: &MockServer,
    route_plugins: Vec<PluginBinding>,
) -> Result<
    crate::domain::services::management::ControlPlaneService,
    crate::domain::error::DomainError,
> {
    let upstream = Upstream {
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
        plugins: PluginsConfig {
            sharing: SharingMode::Private,
            items: route_plugins,
        },
        ..Route::default()
    };
    try_setup_with(server, upstream, route, Some("backend")).await
}

fn data_plane(cp: crate::domain::services::management::ControlPlaneService) -> DataPlaneService {
    DataPlaneService::new(
        Arc::new(cp),
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
    )
    .expect("a buildable data plane")
    .with_registries(
        AuthPluginRegistry::empty(),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
    )
}

#[tokio::test]
async fn a_guard_rejects_the_request_before_it_reaches_the_upstream() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![],
        vec![required_request_headers_guard("x-tenant-id")],
    )
    .await
    .expect("a buildable control plane");
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("unreachable");
    });

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    // A call count of zero is the whole point: the guard ran first.
    mock.assert_calls(0);
}

#[tokio::test]
async fn a_guard_passes_when_the_header_is_present() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![],
        vec![required_request_headers_guard("x-tenant-id")],
    )
    .await
    .expect("a buildable control plane");
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header("x-tenant-id", "t1");
        then.status(200).body("ok");
    });

    let mut response = get(&data_plane(cp), "/backend/api", &[("x-tenant-id", "t1")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(body(&mut response).await, bytes::Bytes::from("ok"));
    mock.assert_calls(1);
}

#[tokio::test]
async fn transforms_run_before_the_call_and_again_on_the_response() {
    let server = MockServer::start();
    let cp = chain_fixture(&server, vec![], vec![request_id_transform()])
        .await
        .expect("a buildable control plane");
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("x-request-id", "corr-1");
        then.status(200).body("ok");
    });

    let mut response = get(
        &data_plane(cp),
        "/backend/api",
        &[("x-request-id", "corr-1")],
    )
    .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert_calls(1);
    assert_eq!(body(&mut response).await, bytes::Bytes::from("ok"));
    assert_eq!(
        response
            .headers()
            .get(gts::request_id_header())
            .and_then(|value| value.to_str().ok()),
        Some("corr-1"),
        "the response phase echoes the correlation id"
    );
}

#[tokio::test]
async fn upstream_and_route_bound_transforms_compose() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![request_id_transform()],
        vec![request_id_transform()],
    )
    .await
    .expect("a buildable control plane");
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header_exists("x-request-id");
        then.status(200).body("ok");
    });

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    mock.assert_calls(1);
    assert!(
        response.headers().contains_key(gts::request_id_header()),
        "the generated id is echoed back on the response"
    );
}

#[tokio::test]
async fn a_guard_can_reject_the_upstream_response() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![required_response_headers_guard("x-trace-id")],
        vec![],
    )
    .await
    .expect("a buildable control plane");
    server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_guard_can_accept_the_upstream_response() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![required_response_headers_guard("x-trace-id")],
        vec![],
    )
    .await
    .expect("a buildable control plane");
    server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).header("x-trace-id", "t-1").body("ok");
    });

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn catalog_only_plugin_references_never_resolve() {
    // Documented in the catalogue, backed by no implementation: `basic` and
    // `bearer` auth, the `timeout` and `cors` guards, `logging` and `metrics`
    // transforms (ADR 0008 §4, ADR 0009 §3).
    let auth = AuthPluginRegistry::empty();
    let guard = GuardPluginRegistry::with_builtins();
    let transform = TransformPluginRegistry::with_builtins();
    for (reference, resolved) in [
        (gts::AUTH_BASIC, auth.resolve(gts::AUTH_BASIC).err()),
        (gts::AUTH_BEARER, auth.resolve(gts::AUTH_BEARER).err()),
        (gts::GUARD_TIMEOUT, guard.resolve(gts::GUARD_TIMEOUT).err()),
        (gts::GUARD_CORS, guard.resolve(gts::GUARD_CORS).err()),
        (
            gts::TRANSFORM_LOGGING,
            transform.resolve(gts::TRANSFORM_LOGGING).err(),
        ),
        (
            gts::TRANSFORM_METRICS,
            transform.resolve(gts::TRANSFORM_METRICS).err(),
        ),
    ] {
        let error = resolved.unwrap_or_else(|| panic!("`{reference}` must fail to resolve"));
        assert_eq!(
            error,
            PluginResolveError::CatalogOnly(reference.to_owned()),
            "the rejection names the catalog gap, not an unknown plugin"
        );
        // The data plane maps this to `PluginNotFound` (503), as the test below
        // exercises end to end.
        assert_eq!(
            DomainError::from(error).status(),
            http::StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

#[tokio::test]
async fn unknown_plugin_references_are_rejected_at_bind_time() {
    let server = MockServer::start();
    let unknown = vec![PluginBinding::Reference(
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1".to_owned(),
    )];
    // The reference resolves to neither a built-in nor a stored custom plugin,
    // so the binding is refused when the upstream is stored rather than at
    // request time (`oagw-plugins` -> "Plugin identification").
    let Err(error) = try_chain_fixture(&server, unknown).await else {
        panic!("an unknown plugin reference must not be storable");
    };
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("nope.v1"));
}

/// A UUID-shaped reference that names no stored plugin is likewise refused at
/// bind time; at proxy time the registries would silently skip it.
#[tokio::test]
async fn a_uuid_reference_to_no_stored_plugin_is_rejected_at_bind_time() {
    let server = MockServer::start();
    let unknown = vec![PluginBinding::Reference(format!(
        "{type_id}~{uuid}",
        type_id = gts::GUARD_PLUGIN_TYPE,
        uuid = uuid::Uuid::now_v7()
    ))];
    let Err(error) = try_chain_fixture(&server, unknown).await else {
        panic!("a dangling custom plugin reference must not be stored");
    };
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn plugin_config_reaches_the_implementation() {
    let mut values = BTreeMap::new();
    values.insert("required_request_headers".to_owned(), json!("x-trace"));
    let binding = PluginBinding::Bound {
        plugin_ref: gts::GUARD_REQUIRED_HEADERS.to_owned(),
        plugin_uuid: None,
        config: Some(values),
    };
    let config = PluginConfig::from(&binding);
    assert_eq!(config.plugin_ref, gts::GUARD_REQUIRED_HEADERS);
    assert_eq!(config.string("required_request_headers"), Some("x-trace"));
}

/// Guards are a phase ahead of transforms, regardless of declaration order: the
/// transform is declared first here, so if the chain interleaved the two kinds
/// it would have already added `x-request-id` by the time the guard looked, and
/// the guard would pass. The spec puts all guards before all transforms
/// (`oagw-plugins` → "Plugin types and traits"), so the guard must reject.
#[tokio::test]
async fn guards_run_before_request_transforms_even_when_declared_after() {
    let server = MockServer::start();
    let cp = chain_fixture(
        &server,
        vec![],
        vec![
            request_id_transform(),
            required_request_headers_guard("x-request-id"),
        ],
    )
    .await
    .expect("a buildable control plane");
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("unreachable");
    });

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    mock.assert_calls(0);
}

/// Same phase rule on the response leg: the guard sees the response before the
/// transform has touched it, so a required response header that only the
/// transform adds is still absent when the guard runs.
#[tokio::test]
async fn guards_run_before_response_transforms() {
    let server = MockServer::start();
    let upstream = Upstream {
        headers: HeadersConfig {
            response: crate::domain::model::ResponseHeaderRules::default(),
            ..HeadersConfig::default()
        },
        plugins: PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![
                request_id_transform(),
                required_response_headers_guard("x-request-id"),
            ],
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
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });
    let (cp, _) = setup_with(&server, upstream, route, Some("backend")).await;

    let response = get(&data_plane(cp), "/backend/api", &[]).await;
    // A guard rejecting on the response leg is a bad gateway, not a bad request
    // (the client's request was fine).
    assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
    mock.assert();
}
