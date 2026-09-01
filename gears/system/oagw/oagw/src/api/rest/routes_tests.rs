#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use toolkit::api::{OpenApiRegistry, OpenApiRegistryImpl};
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use super::register_routes;
use crate::domain::model::{GUARD_PLUGIN_TYPE, UPSTREAM_TYPE, canonical_types};
use crate::domain::repo::{
    AllowAllAuthorizer, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::services::ControlPlaneService;
use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use crate::infra::plugin::registry::AuthPluginRegistry;
use crate::infra::plugin::secrets::StaticSecretResolver;
use crate::infra::proxy::forward::ProxyEngine;
use crate::infra::proxy::policy::ProxyPolicy;
use crate::infra::proxy::resolver::StaticHierarchy;
use crate::infra::proxy::ssrf::SsrfGuard;
use crate::infra::proxy::transport::UpstreamTransport;
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

/// An engine over empty repositories, for the route-registration tests.
fn engine() -> Arc<ProxyEngine> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    let policy = ProxyPolicy::new(5, true);
    let cache_config = TokenCacheConfig::default();
    Arc::new(ProxyEngine::new(
        upstreams as Arc<dyn UpstreamRepository>,
        routes as Arc<dyn RouteRepository>,
        Arc::new(StaticHierarchy::new(Vec::new())),
        AuthPluginRegistry::with_builtins(
            Arc::new(StaticSecretResolver::default()),
            None,
            cache_config,
        ),
        SsrfGuard::disabled(),
        policy,
        UpstreamTransport::new(&policy).unwrap(),
    ))
}

/// Tenant of the test router's synthetic caller.
const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0xA001);

fn security(tenant: uuid::Uuid) -> SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(0xC003))
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

/// Every gear-relative path the gear registers.
const EXPECTED_PATHS: &[&str] = &[
    "/oagw/v1/upstreams",
    "/oagw/v1/upstreams/{id}",
    "/oagw/v1/routes",
    "/oagw/v1/routes/{id}",
    "/oagw/v1/plugins",
    "/oagw/v1/plugins/{id}",
    "/oagw/v1/plugins/{id}/source",
    "/oagw/v1/proxy/{alias}",
    "/oagw/v1/proxy/{alias}/{*path_suffix}",
];

/// The methods the data plane documents per proxy path.
const PROXY_METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

/// The operation ids `DESIGN` §3.1 documents.
///
/// The data plane is mounted once per path and declared once per documented
/// method, so each proxy path contributes `PROXY_METHODS.len()` operation ids.
const OPERATION_IDS: &[&str] = &[
    "oagw.create_upstream",
    "oagw.list_upstreams",
    "oagw.get_upstream",
    "oagw.replace_upstream",
    "oagw.delete_upstream",
    "oagw.create_route",
    "oagw.list_routes",
    "oagw.get_route",
    "oagw.replace_route",
    "oagw.delete_route",
    "oagw.create_plugin",
    "oagw.list_plugins",
    "oagw.get_plugin",
    "oagw.get_plugin_source",
    "oagw.delete_plugin",
    "oagw.proxy.invoke.get",
    "oagw.proxy.invoke.post",
    "oagw.proxy.invoke.put",
    "oagw.proxy.invoke.delete",
    "oagw.proxy.invoke.patch",
    "oagw.proxy.invoke_with_suffix.get",
    "oagw.proxy.invoke_with_suffix.post",
    "oagw.proxy.invoke_with_suffix.put",
    "oagw.proxy.invoke_with_suffix.delete",
    "oagw.proxy.invoke_with_suffix.patch",
];

#[test]
fn every_proxy_method_is_declared_for_both_proxy_paths() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    let document = serde_json::to_value(
        openapi
            .build_openapi(&toolkit::api::OpenApiInfo::default())
            .unwrap(),
    )
    .unwrap();
    for (path, suffix) in [
        ("/oagw/v1/proxy/{alias}", ""),
        ("/oagw/v1/proxy/{alias}/{*path_suffix}", "_with_suffix"),
    ] {
        let openapi_path = path.replace("{*path_suffix}", "{path_suffix}");
        for method in PROXY_METHODS {
            let operation = &document["paths"][&openapi_path][method];
            assert!(operation.is_object(), "{openapi_path} declares no {method}");
            assert_eq!(
                operation["operationId"].as_str().unwrap(),
                format!("oagw.proxy.invoke{suffix}.{method}")
            );
        }
    }
}

#[test]
fn the_plugins_filter_example_names_the_plugin_type_alias() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    let document = serde_json::to_value(
        openapi
            .build_openapi(&toolkit::api::OpenApiInfo::default())
            .unwrap(),
    )
    .unwrap();
    let parameters = &document["paths"]["/oagw/v1/plugins"]["get"]["parameters"];
    let filter = parameters
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "$filter")
        .expect("the plugins list declares $filter");
    let description = filter["description"].as_str().unwrap();
    assert!(description.contains("type eq 'guard'"), "{description}");
    assert!(!description.contains("alias eq"), "{description}");
}

fn service() -> Arc<ControlPlaneService> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    let plugins = Arc::new(MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
        Arc::clone(&routes) as Arc<dyn RouteRepository>,
    ));
    Arc::new(ControlPlaneService::new(
        upstreams as Arc<dyn UpstreamRepository>,
        routes as Arc<dyn RouteRepository>,
        plugins as Arc<dyn PluginRepository>,
        Arc::new(AllowAllAuthorizer),
        50,
        100,
    ))
}

/// A registry that records the registered operation ids and paths.
#[derive(Default)]
struct RecordingRegistry {
    operation_ids: std::sync::Mutex<Vec<String>>,
    paths: std::sync::Mutex<Vec<String>>,
    schemas: std::sync::Mutex<Vec<String>>,
}

impl OpenApiRegistry for RecordingRegistry {
    fn register_operation(&self, spec: &toolkit::api::OperationSpec) {
        if let Some(operation_id) = &spec.operation_id {
            self.operation_ids
                .lock()
                .unwrap()
                .push(operation_id.clone());
        }
        self.paths.lock().unwrap().push(spec.path.clone());
    }

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        self.schemas.lock().unwrap().push(name.to_owned());
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn router() -> Router {
    let openapi = OpenApiRegistryImpl::new();
    let routes = register_routes(Router::new(), &openapi, service(), engine());
    // The platform installs the caller's `SecurityContext` as an extension
    // before the router runs; this test router has to supply its own.
    routes.layer(Extension(security(TENANT)))
}

// ── registration ───────────────────────────────────────────────────────────

#[test]
fn every_operation_is_registered_with_the_documented_operation_id() {
    let registry = RecordingRegistry::default();
    let _router = register_routes(Router::new(), &registry, service(), engine());
    let ids = registry.operation_ids.lock().unwrap().clone();
    for expected in OPERATION_IDS {
        assert!(ids.iter().any(|id| id == expected), "missing {expected}");
    }
    assert_eq!(ids.len(), OPERATION_IDS.len(), "no undocumented operation");
}

#[test]
fn no_route_path_writes_the_operators_api_prefix() {
    let registry = RecordingRegistry::default();
    let _router = register_routes(Router::new(), &registry, service(), engine());

    let paths = registry.paths.lock().unwrap().clone();
    let mut mounted = paths.clone();
    mounted.sort();
    mounted.dedup();
    assert_eq!(mounted.len(), EXPECTED_PATHS.len(), "one mount per path");
    assert_eq!(
        paths.len(),
        OPERATION_IDS.len(),
        "one spec per operation id"
    );
    // The `/api` segment belongs to the platform gateway, not to this gear, so
    // a gear-relative path must never contain it.
    for path in &paths {
        assert!(path.starts_with("/oagw/"), "{path}");
        assert!(!path.contains("/api/"), "{path}");
    }
    // And the paths are exactly the documented ones.
    let mut unique = paths;
    unique.sort();
    unique.dedup();
    let mut expected = EXPECTED_PATHS.to_vec();
    expected.sort_unstable();
    assert_eq!(unique, expected);
}

#[test]
fn the_response_dtos_are_registered_as_components() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    let components = openapi.components_registry.load();
    for name in [
        "UpstreamRequestDto",
        "UpstreamResponseDto",
        "RouteRequestDto",
        "RouteResponseDto",
        "PluginRequestDto",
        "PluginResponseDto",
        "PluginSourceResponseDto",
        "ListEnvelopeDto",
    ] {
        assert!(
            components.contains_key(name),
            "component `{name}` must be registered"
        );
    }
}

// ── wire behaviour ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_proxy_route_accepts_every_method() {
    // The route is mounted once with `routing::any`, so every method reaches
    // the data plane: the empty repositories answer the same unknown-alias
    // routing miss, with the gateway as its source, for all of them.
    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
        let request = Request::builder()
            .method(method.clone())
            .uri("/oagw/v1/proxy/api.openai.com/v1/models")
            .body(Body::empty())
            .unwrap();
        let response = router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            document["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "{method}"
        );
        assert_eq!(document["status"], serde_json::json!(404), "{method}");
    }
}

/// The single value of a response header, when it is legible as text.
fn header_value(response: &axum::http::Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
async fn the_proxy_route_matches_the_alias_only_form() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/proxy/api.openai.com")
        .body(Body::empty())
        .unwrap();
    let response = router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header_value(&response, "x-oagw-error-source").as_deref(),
        Some("gateway")
    );
}

#[tokio::test]
async fn an_unknown_management_path_is_404() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/unknown")
        .body(Body::empty())
        .unwrap();
    let response = router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── OpenAPI document ───────────────────────────────────────────────────────

#[test]
fn the_generated_document_declares_the_gear_relative_paths() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    let document = openapi
        .build_openapi(&toolkit::api::OpenApiInfo {
            title: "OAGW".to_owned(),
            version: "1".to_owned(),
            description: None,
            servers: Vec::new(),
        })
        .unwrap();
    let rendered = serde_json::to_value(&document).unwrap();
    for path in [
        "/oagw/v1/upstreams",
        "/oagw/v1/upstreams/{id}",
        "/oagw/v1/routes",
        "/oagw/v1/routes/{id}",
        "/oagw/v1/plugins",
        "/oagw/v1/plugins/{id}",
        "/oagw/v1/plugins/{id}/source",
        "/oagw/v1/proxy/{alias}",
        "/oagw/v1/proxy/{alias}/{*path_suffix}",
    ] {
        let openapi_path = path.replace("{*path_suffix}", "{path_suffix}");
        assert!(
            rendered["paths"].get(&openapi_path).is_some(),
            "missing path {openapi_path}"
        );
    }
}

#[test]
fn every_list_operation_declares_the_five_system_options() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    for (path, method) in [
        ("/oagw/v1/upstreams", "get"),
        ("/oagw/v1/routes", "get"),
        ("/oagw/v1/plugins", "get"),
    ] {
        let document = serde_json::to_value(
            openapi
                .build_openapi(&toolkit::api::OpenApiInfo::default())
                .unwrap(),
        )
        .unwrap();
        let operation = &document["paths"][path][method];
        let names: Vec<&str> = operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|parameter| parameter["name"].as_str())
            .collect();
        for option in ["$filter", "$select", "$orderby", "$top", "$skip"] {
            assert!(
                names.contains(&option),
                "{path} is missing the {option} parameter: {names:?}"
            );
        }
    }
}

#[test]
fn the_documented_problem_responses_are_declared() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi, service(), engine());
    let document = serde_json::to_value(
        openapi
            .build_openapi(&toolkit::api::OpenApiInfo::default())
            .unwrap(),
    )
    .unwrap();

    let create = &document["paths"]["/oagw/v1/upstreams"]["post"]["responses"];
    assert!(create["201"].is_object());
    assert!(
        create["409"].is_object(),
        "an alias conflict must be declared"
    );

    let delete_plugin = &document["paths"]["/oagw/v1/plugins/{id}"]["delete"]["responses"];
    assert!(
        delete_plugin["409"].is_object(),
        "an in-use plugin must be declarable"
    );
    assert!(delete_plugin["204"].is_object());

    let proxy = &document["paths"]["/oagw/v1/proxy/{alias}"]["get"]["responses"];
    assert!(proxy["503"].is_object());
    assert!(
        proxy["503"]["content"]["application/problem+json"].is_object(),
        "the problem media type must be declared"
    );
}

#[test]
fn the_canonical_error_identities_are_the_documented_spellings() {
    assert_eq!(
        canonical_types::NOT_FOUND,
        "gts.cf.core.errors.err.v1~cf.core.err.not_found.v1"
    );
    assert_eq!(GUARD_PLUGIN_TYPE, "gts.cf.core.oagw.guard_plugin.v1~");
    assert!(UPSTREAM_TYPE.starts_with("gts.cf.core.oagw.upstream.v1~"));
}
