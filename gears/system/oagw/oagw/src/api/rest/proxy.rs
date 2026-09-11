//! Proxy API routes.
//!
//! Implemented by DECOMPOSITION entries 2.5 (proxy-core: alias/route
//! resolution and plain HTTP forwarding under
//! `/oagw/v1/proxy/{alias}[/{path_suffix}]`) and 2.6 (streaming and
//! protocol upgrades layered on top of the same path).

use std::sync::Arc;

use axum::extract::Request;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;

use crate::proxy::endpoint::RoundRobinState;
use crate::proxy::engine::{self, ProxyDeps};
use crate::proxy::forward::LazyHttpClient;
use crate::proxy::hierarchy::{NoTenantHierarchy, TenantHierarchyProvider};
use crate::store::OagwState;

/// `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
/// (`cpt-cf-oagw-dod-proxy-endpoint-registration`): a single axum route
/// captures the whole `{alias}[/{path_suffix}]` tail via `{*rest}` and the
/// engine re-splits it exactly per `cpt-cf-oagw-algo-proxy-parse-request`,
/// so alias/suffix segmentation lives in one place.
const PROXY_WILDCARD_PATH: &str = "/oagw/v1/proxy/{*rest}";

/// `Extension<Arc<OagwState>>`, `Extension<Arc<dyn TenantHierarchyProvider>>`,
/// `Extension<Arc<LazyHttpClient>>` and `Extension<Arc<RoundRobinState>>` are
/// layered onto the router by [`register_routes`]; `SecurityContext` is
/// supplied by platform middleware before this handler runs
/// (`inst-proxy-fwd-receive`).
async fn handle(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(hierarchy): Extension<Arc<dyn TenantHierarchyProvider>>,
    Extension(http_client): Extension<Arc<LazyHttpClient>>,
    Extension(round_robin): Extension<Arc<RoundRobinState>>,
    Extension(ctx): Extension<SecurityContext>,
    req: Request,
) -> Response {
    let deps = ProxyDeps {
        state: &state,
        hierarchy: hierarchy.as_ref(),
        http_client: http_client.client(),
        round_robin: &round_robin,
    };
    engine::handle_proxy_request(deps, &ctx, req).await
}

pub(crate) fn register_routes(
    router: Router,
    _openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    register_routes_with_hierarchy(router, state, Arc::new(NoTenantHierarchy))
}

/// Same wiring as [`register_routes`], parameterized over the
/// [`TenantHierarchyProvider`] -- production always uses
/// [`NoTenantHierarchy`] (see `crate::proxy::hierarchy`'s doc comment);
/// tests inject a fake provider to exercise the ancestor-chain branches
/// end to end through a real router.
fn register_routes_with_hierarchy(
    router: Router,
    state: Arc<OagwState>,
    hierarchy: Arc<dyn TenantHierarchyProvider>,
) -> Router {
    let http_client = Arc::new(LazyHttpClient::default());
    let round_robin = Arc::new(RoundRobinState::new());

    router
        .route(
            PROXY_WILDCARD_PATH,
            get(handle)
                .post(handle)
                .put(handle)
                .delete(handle)
                .patch(handle)
                .options(handle),
        )
        .layer(Extension(round_robin))
        .layer(Extension(http_client))
        .layer(Extension(hierarchy))
        .layer(Extension(state))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::route::{HttpMatch, PathSuffixMode, Route, RouteMatch};
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig, Upstream};
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use http_body_util::BodyExt;
    use httpmock::prelude::*;
    use serde_json::Value;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn ctx(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap()
    }

    fn build_router() -> (Router, Arc<OagwState>) {
        let config = OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let state = Arc::new(OagwState::new(config));
        let registry = toolkit::api::OpenApiRegistryImpl::new();
        let router = register_routes(Router::new(), &registry, state.clone());
        (router, state)
    }

    fn seed_upstream(state: &OagwState, tenant_id: Uuid, alias: &str, port: u16) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    fn seed_route(
        state: &OagwState,
        upstream_id: Uuid,
        path: &str,
        methods: Vec<crate::model::route::HttpMethod>,
    ) {
        let route = Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Uuid::new_v4(),
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods,
                    path: path.to_owned(),
                    query_allowlist: vec!["q".to_owned()],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: Some(1),
        };
        state
            .store
            .routes()
            .insert(route.id.unwrap(), Arc::new(route));
    }

    fn request(method: &str, uri: &str, tenant_id: Uuid) -> HttpRequest<Body> {
        let mut req = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ctx(tenant_id));
        req
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn successful_get_reaches_upstream_and_relays_response() {
        let server = MockServer::start();
        let _m = server.mock(|when, then| {
            when.method(GET).path("/v1/models/extra");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        });

        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id, "test-svc", server.port());
        seed_route(
            &state,
            upstream_id,
            "/v1/models",
            vec![crate::model::route::HttpMethod::Get],
        );

        let response = router
            .oneshot(request(
                "GET",
                "/oagw/v1/proxy/test-svc/v1/models/extra",
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );
        let body = body_bytes(response).await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
    }

    #[tokio::test]
    async fn unresolved_alias_returns_404_route_not_found() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let response = router
            .oneshot(request("GET", "/oagw/v1/proxy/nope", tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_bytes(response).await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn disabled_upstream_returns_503() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: false,
            alias: Some("disabled-svc".to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 1,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));

        let response = router
            .oneshot(request("GET", "/oagw/v1/proxy/disabled-svc", tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[tokio::test]
    async fn non_allowlisted_query_param_is_rejected_before_reaching_upstream() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.path("/v1/models");
            then.status(200);
        });

        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id, "q-svc", server.port());
        seed_route(
            &state,
            upstream_id,
            "/v1/models",
            vec![crate::model::route::HttpMethod::Get],
        );

        let response = router
            .oneshot(request(
                "GET",
                "/oagw/v1/proxy/q-svc/v1/models?bad=1",
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn upstream_418_is_relayed_verbatim_with_upstream_error_source() {
        let server = MockServer::start();
        let _m = server.mock(|when, then| {
            when.path("/");
            then.status(418).body("i am a teapot");
        });

        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id, "teapot-svc", server.port());
        seed_route(
            &state,
            upstream_id,
            "/",
            vec![crate::model::route::HttpMethod::Get],
        );

        let response = router
            .oneshot(request("GET", "/oagw/v1/proxy/teapot-svc", tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::from_u16(418).unwrap());
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );
        let body = body_bytes(response).await;
        assert_eq!(body, b"i am a teapot");
    }

    #[tokio::test]
    async fn connection_refused_returns_503() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id, "dead-svc", 9);
        seed_route(
            &state,
            upstream_id,
            "/v1",
            vec![crate::model::route::HttpMethod::Get],
        );

        let response = router
            .oneshot(request("GET", "/oagw/v1/proxy/dead-svc/v1", tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
