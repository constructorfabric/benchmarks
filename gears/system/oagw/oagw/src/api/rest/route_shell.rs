//! The closed `/oagw/v1` route shell (`cpt-cf-oagw-flow-gear-bootstrap`).
//!
//! The shell is this feature's registration surface: every management and proxy
//! path of the gateway is mounted here, the 15 management methods served by the
//! control-plane handlers of `cpt-cf-oagw-feature-management-api` and the 12
//! proxy methods served by the handlers of
//! `cpt-cf-oagw-feature-proxy-pipeline` over the data-plane pipeline they call.
//! The gear owns no listener (`cpt-cf-oagw-constraint-toolkit-deploy`), so the
//! registration call is the only place that can refuse a duplicate prefix:
//! [`MountLedger`] claims `/oagw/v1` before a single route is registered and
//! refuses a second claim instead of shadowing another gear's routes.
// @cpt-begin:cpt-cf-oagw-dod-gear-registration:p1:inst-full

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::middleware;
use parking_lot::Mutex;

use crate::api::control_plane::{ControlPlaneService, control_plane_methods};
use crate::api::rest::error::error_mapping_middleware;
use crate::api::rest::proxy_handler::{proxy_alias, proxy_path};
use crate::domain::error::OagwError;
use crate::infra::proxy::pipeline::ProxyPipeline;

/// The gear-relative mount prefix of every oagw route (DECOMPOSITION correction
/// 1): no leading `/api` segment is ever registered.
pub const MOUNT_PREFIX: &str = "/oagw/v1";

/// Owning feature of the 15 management handlers.
pub const MANAGEMENT_API_FEATURE: &str = "cpt-cf-oagw-feature-management-api";
/// Owning feature of the proxy handlers.
pub const PROXY_PIPELINE_FEATURE: &str = "cpt-cf-oagw-feature-proxy-pipeline";

/// HTTP methods the proxy shell accepts (DECOMPOSITION entry 2.8).
///
/// `OPTIONS` is part of the closed set so a browser preflight reaches the
/// pipeline's preflight branch and is answered with the permissive 204 ADR 0004
/// decides instead of a routing-layer 405; a non-preflight `OPTIONS` falls
/// through to the route match and is answered `404 RouteNotFound` by the
/// existing row, which is the answer no route of the shell gives it either way.
pub const PROXY_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The closed management route shell under [`MOUNT_PREFIX`], as
/// `path -> accepted methods` (DECOMPOSITION entry 2.4): the 15 management
/// paths, and no method beyond them, so an unregistered method on a mounted
/// path is answered by the routing layer itself.
pub const MANAGEMENT_SHELL: &[(&str, &[&str])] = &[
    ("/upstreams", &["POST", "GET"]),
    ("/upstreams/{id}", &["GET", "PUT", "DELETE"]),
    ("/routes", &["POST", "GET"]),
    ("/routes/{id}", &["GET", "PUT", "DELETE"]),
    ("/plugins", &["POST", "GET"]),
    ("/plugins/{id}", &["GET", "DELETE"]),
    ("/plugins/{id}/source", &["GET"]),
];

/// The closed proxy route shell under [`MOUNT_PREFIX`], as
/// `path -> accepted methods` (DECOMPOSITION entry 2.8, PRD
/// `cpt-cf-oagw-interface-proxy-api`).
pub const PROXY_SHELL: &[(&str, &[&str])] = &[
    ("/proxy/{alias}", PROXY_METHODS),
    ("/proxy/{alias}/{*path}", PROXY_METHODS),
];

/// The exact `(method, path)` pairs of the shell, in registration order.
#[must_use]
pub fn shell_routes() -> Vec<(&'static str, &'static str)> {
    let mut routes = Vec::with_capacity(27);
    for (path, methods) in MANAGEMENT_SHELL.iter().chain(PROXY_SHELL.iter()) {
        for method in *methods {
            routes.push((*method, *path));
        }
    }
    routes
}

/// Ledger of the route prefixes a registration call has already claimed.
///
/// axum's router exposes no route enumeration, so a registration cannot ask the
/// host router whether `/oagw/v1` is free. The ledger records the prefixes the
/// registration calls of this process claimed and refuses a clashing claim
/// before any route is registered; a mount that bypasses the ledger is still
/// rejected by the host router when the conflicting routes are merged.
#[derive(Default)]
pub struct MountLedger {
    claimed: Mutex<Vec<String>>,
}

impl MountLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide ledger every registration call claims its prefix in.
    #[must_use]
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<MountLedger> = OnceLock::new();
        SHARED.get_or_init(MountLedger::new)
    }

    /// Claims `prefix`, refusing a prefix that is already mounted or that would
    /// shadow a mounted one.
    ///
    /// # Errors
    /// Returns the typed startup error surface — a `ValidationError` naming the
    /// clashing prefix — when the prefix is already mounted.
    pub fn claim(&self, prefix: &str) -> Result<(), OagwError> {
        let mut claimed = self.claimed.lock();
        if let Some(existing) = claimed.iter().find(|existing| shadows(existing, prefix)) {
            return Err(OagwError::validation_error(format!(
                "oagw.routes: route prefix '{prefix}' is already mounted by '{existing}'; the registration refuses to shadow it"
            )));
        }
        claimed.push(prefix.to_owned());
        Ok(())
    }

    /// The claimed prefixes, in claim order.
    #[must_use]
    pub fn claims(&self) -> Vec<String> {
        self.claimed.lock().clone()
    }
}

/// Reports whether two route prefixes would mount over each other: they are
/// equal, or one is a whole path-segment prefix of the other.
fn shadows(existing: &str, candidate: &str) -> bool {
    let (shorter, longer) = if existing.len() <= candidate.len() {
        (existing, candidate)
    } else {
        (candidate, existing)
    };
    longer == shorter || (longer.starts_with(shorter) && longer.as_bytes()[shorter.len()] == b'/')
}

/// Registers the closed `/oagw/v1` shell on `router` and returns the merged
/// router.
///
/// The 15 management methods are served by the control-plane handlers of
/// `control_plane`, and the 12 proxy methods by the handlers of
/// `proxy_handler` over the data-plane pipeline the gear holds. The shell is
/// built as its own router so the error layer applies to oagw routes only, and
/// is then merged into the host router the gear was handed.
///
/// # Errors
/// Returns the typed startup error surface when [`MOUNT_PREFIX`] is already
/// mounted, before a single route is registered.
pub fn mount(
    router: Router,
    ledger: &MountLedger,
    control_plane: Arc<ControlPlaneService>,
    proxy: Arc<ProxyPipeline>,
) -> Result<Router, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-12
    // The refusal is part of the registration call itself, not a
    // post-registration check: the prefix is claimed before any route mounts.
    ledger.claim(MOUNT_PREFIX)?;
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-12

    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-11
    // Register the exact shell set under /oagw/v1/...: the 15 management paths
    // served by the control-plane handlers of
    // `cpt-cf-oagw-feature-management-api`, and the proxy shell served by the
    // handlers of `cpt-cf-oagw-feature-proxy-pipeline`, which own its handler
    // body.
    let mut shell = Router::new();
    for (path, methods) in MANAGEMENT_SHELL.iter() {
        shell = shell.route(
            &full_path(path),
            control_plane_methods(path, methods)?.with_state(Arc::clone(&control_plane)),
        );
    }
    // The buffered request body is bounded by the same limit the pipeline
    // enforces on a declared `Content-Length`: the body-limit knob of the
    // configuration the pipeline was built from, read off the pipeline itself,
    // so no second reader of the knob can drift from it. The layer is on the
    // proxy routes alone, a management body being no upstream request body.
    let body_limit = usize::try_from(proxy.body_limit()).unwrap_or(usize::MAX);
    let mut proxy_shell = Router::new();
    proxy_shell = proxy_shell
        .route(
            &full_path("/proxy/{alias}"),
            proxy_methods(proxy_alias)?.with_state(Arc::clone(&proxy)),
        )
        .route(
            &full_path("/proxy/{alias}/{*path}"),
            proxy_methods(proxy_path)?.with_state(proxy),
        )
        .layer(axum::extract::DefaultBodyLimit::max(body_limit));
    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-13
    // The shell leaves no 404 `RouteNotFound` placeholder of its own: each of
    // the paths above is handed to the owning feature's handler, and a path the
    // closed shell does not mount stays with the routing layer, which answers it
    // with the 404 `RouteNotFound` row of the error mapping.
    shell = shell.merge(proxy_shell);
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-13

    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-10
    // The OagwError to problem+json mapper is the REST error layer of every
    // mounted route.
    let shell = shell.layer(middleware::from_fn(error_mapping_middleware));
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-10

    Ok(router.merge(shell))
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-11
}

/// Builds the `MethodRouter` of one proxy shell path, answering exactly the
/// six methods `PROXY_METHODS` accepts; the routing layer answers every other
/// method itself. The two shell paths each carry their own handler over the
/// shared pipeline state.
fn proxy_methods<H, T>(
    handler: H,
) -> Result<axum::routing::MethodRouter<Arc<ProxyPipeline>>, OagwError>
where
    H: axum::handler::Handler<T, Arc<ProxyPipeline>> + Clone + Send + Sync + 'static,
    T: 'static,
{
    let mut router = axum::routing::MethodRouter::new();
    for method in PROXY_METHODS {
        let method = *method;
        router = match method {
            "GET" => router.get(handler.clone()),
            "POST" => router.post(handler.clone()),
            "PUT" => router.put(handler.clone()),
            "PATCH" => router.patch(handler.clone()),
            "OPTIONS" => router.options(handler.clone()),
            "DELETE" => router.delete(handler.clone()),
            other => {
                return Err(OagwError::route_error(format!(
                    "oagw.routes: method '{other}' is not part of the closed route shell"
                )));
            }
        };
    }
    Ok(router)
}

/// Joins the mount prefix with a shell path.
fn full_path(shell_path: &str) -> String {
    format!("{MOUNT_PREFIX}{shell_path}")
}

// @cpt-end:cpt-cf-oagw-dod-gear-registration:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::error::{ERROR_SOURCE_GATEWAY, PROBLEM_JSON, X_OAGW_ERROR_SOURCE};
    use crate::infra::proxy::pipeline::ProxyLimits;
    use crate::infra::proxy::pipeline::stub::{
        StubConnector, pipeline as proxy_pipeline, pipeline_with_limits,
    };
    use crate::infra::storage::InMemoryStores;
    use axum::body::to_bytes as read_body;
    use axum::http::Request as HttpRequest;
    use serde_json::Value;
    use tower::ServiceExt;

    /// The exact shell set of `cpt-cf-oagw-dod-gear-registration`.
    fn expected_routes() -> Vec<(&'static str, &'static str)> {
        let management: &[(&str, &str)] = &[
            ("POST", "/upstreams"),
            ("GET", "/upstreams"),
            ("GET", "/upstreams/{id}"),
            ("PUT", "/upstreams/{id}"),
            ("DELETE", "/upstreams/{id}"),
            ("POST", "/routes"),
            ("GET", "/routes"),
            ("GET", "/routes/{id}"),
            ("PUT", "/routes/{id}"),
            ("DELETE", "/routes/{id}"),
            ("POST", "/plugins"),
            ("GET", "/plugins"),
            ("GET", "/plugins/{id}"),
            ("DELETE", "/plugins/{id}"),
            ("GET", "/plugins/{id}/source"),
        ];
        let mut routes: Vec<(&str, &str)> = management.to_vec();
        for path in ["/proxy/{alias}", "/proxy/{alias}/{*path}"] {
            for method in PROXY_METHODS {
                routes.push((method, path));
            }
        }
        routes
    }

    /// Substitutes the `{id}` / `{alias}` / `{path}` captures of a shell path
    /// and joins the mount prefix, giving the request path the shell serves.
    fn concrete_path(shell_path: &str) -> String {
        let concrete = shell_path
            .replace("{id}", "identity-1")
            .replace("{alias}", "payments")
            .replace("{*path}", "accounts/42")
            .replace("{path}", "accounts/42");
        full_path(&concrete)
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = read_body(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The shell mounted the way the gear mounts it: over a control plane on
    /// empty stores and over the data plane built from those same stores.
    fn mounted() -> Router {
        mounted_with_control_plane(Arc::new(ControlPlaneService::default()))
    }

    /// The shell mounted over an explicit control plane, whose stores the data
    /// plane the proxy handlers drive is built from.
    fn mounted_with_control_plane(control_plane: Arc<ControlPlaneService>) -> Router {
        mount(
            Router::new(),
            &MountLedger::new(),
            Arc::clone(&control_plane),
            proxy_pipeline(control_plane.stores(), Arc::new(StubConnector::new())),
        )
        .unwrap()
    }

    /// Sends one request to the mounted shell.
    async fn send(router: Router, method: &str, path: &str) -> axum::response::Response {
        router
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[test]
    fn the_shell_is_the_closed_27_route_set() {
        assert_eq!(shell_routes(), expected_routes());
        assert_eq!(
            MANAGEMENT_SHELL.len(),
            7,
            "7 shell paths carry the 15 management methods"
        );
        let management_methods: usize = MANAGEMENT_SHELL
            .iter()
            .map(|(_, methods)| methods.len())
            .sum();
        assert_eq!(management_methods, 15);
        let proxy_methods: usize = PROXY_SHELL.iter().map(|(_, methods)| methods.len()).sum();
        assert_eq!(
            proxy_methods, 12,
            "2 proxy paths times the 6 accepted methods"
        );
    }

    #[test]
    fn the_shell_mounts_gear_relative_without_an_api_prefix() {
        assert_eq!(MOUNT_PREFIX, "/oagw/v1");
        for (method, path) in shell_routes() {
            let full = full_path(path);
            assert!(
                full.starts_with(MOUNT_PREFIX),
                "{method} {full} must mount under the gear-relative prefix"
            );
            assert!(
                !full.contains("/api/"),
                "{method} {full} must never be registered behind the platform /api prefix"
            );
        }
    }

    /// The 15 management methods are served by the control-plane handlers, so a
    /// request that reaches one without a security context is answered by the
    /// handler itself with the 401 of the existing `AuthenticationFailed` row,
    /// and never by the shell placeholder.
    #[tokio::test]
    async fn every_management_route_answers_its_control_plane_handler() {
        let router = mounted();

        for (method, shell_path) in expected_routes() {
            if shell_path.starts_with("/proxy/") {
                continue;
            }
            let path = concrete_path(shell_path);
            let response = send(router.clone(), method, &path).await;

            assert_eq!(
                response.status(),
                401,
                "{method} {path} must be served by the control-plane handler"
            );
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some(PROBLEM_JSON),
                "{method} {path} must be problem+json"
            );
            assert_eq!(
                response
                    .headers()
                    .get(X_OAGW_ERROR_SOURCE)
                    .and_then(|value| value.to_str().ok()),
                Some(ERROR_SOURCE_GATEWAY)
            );

            let body = body_json(response).await;
            assert_eq!(
                body["type"], "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
                "{method} {path}"
            );
            assert_eq!(body["status"], 401);
            assert_eq!(body["instance"], path);
            assert!(
                !body["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(MANAGEMENT_API_FEATURE),
                "{method} {path} must not be answered by the shell placeholder"
            );
        }
    }

    /// The proxy methods are served by the handlers of
    /// `cpt-cf-oagw-feature-proxy-pipeline`, so a request that reaches one
    /// without a security context is answered by the handler itself with the
    /// 401 of the existing `AuthenticationFailed` row, and never by a
    /// placeholder.
    #[tokio::test]
    async fn every_proxy_route_answers_its_proxy_handler() {
        let router = mounted();

        for (method, shell_path) in expected_routes() {
            if !shell_path.starts_with("/proxy/") {
                continue;
            }
            let path = concrete_path(shell_path);
            let response = send(router.clone(), method, &path).await;

            assert_eq!(
                response.status(),
                401,
                "{method} {path} must be served by the proxy handler"
            );
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some(PROBLEM_JSON),
                "{method} {path} must be problem+json"
            );
            assert_eq!(
                response
                    .headers()
                    .get(X_OAGW_ERROR_SOURCE)
                    .and_then(|value| value.to_str().ok()),
                Some(ERROR_SOURCE_GATEWAY)
            );

            let body = body_json(response).await;
            assert_eq!(
                body["type"], "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
                "{method} {path}"
            );
            assert_eq!(body["status"], 401);
            assert_eq!(body["instance"], path);
        }
    }

    #[tokio::test]
    async fn a_method_outside_the_shell_is_answered_by_the_routing_layer() {
        let router = mounted();

        let response = send(router, "PUT", "/oagw/v1/plugins/plugin-1").await;

        assert_eq!(
            response.status(),
            405,
            "the routing layer answers the unregistered method"
        );
        assert_ne!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROBLEM_JSON),
            "no 405 row is invented in the closed mapping table"
        );
        let bytes = read_body(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            bytes.is_empty(),
            "the routing-layer 405 carries no problem document: {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[tokio::test]
    async fn a_duplicate_prefix_is_refused_by_the_registration_call() {
        let ledger = MountLedger::new();
        let first = mounted_over(&ledger).unwrap();
        assert_eq!(ledger.claims(), vec![MOUNT_PREFIX.to_owned()]);
        assert!(
            first.has_routes(),
            "the first registration mounts its shell"
        );

        let refused = mounted_over(&ledger).unwrap_err();
        assert_eq!(refused.mapping().variant, "ValidationError");
        assert_eq!(refused.status(), 400);
        assert!(
            !refused.is_retriable(),
            "a refused registration is not retriable"
        );
        assert!(
            refused.detail().contains("'/oagw/v1'"),
            "the refused prefix must be named, got: {}",
            refused.detail()
        );
        assert_eq!(
            ledger.claims(),
            vec![MOUNT_PREFIX.to_owned()],
            "the refused registration claims nothing"
        );
    }

    #[tokio::test]
    async fn a_prefix_that_would_shadow_or_be_shadowed_is_refused() {
        let ledger = MountLedger::new();
        ledger.claim("/oagw").unwrap();
        let error = mounted_over(&ledger).unwrap_err();
        assert_eq!(error.mapping().variant, "ValidationError");

        let ledger = MountLedger::new();
        ledger.claim("/oagw/v1/proxy").unwrap();
        let error = mounted_over(&ledger).unwrap_err();
        assert!(error.detail().contains("'/oagw/v1'"), "{}", error.detail());
    }

    #[tokio::test]
    async fn a_fresh_ledger_mounts_the_shell_again() {
        let router = mounted();
        let response = send(router, "GET", "/oagw/v1/proxy/payments").await;
        assert_eq!(
            response.status(),
            401,
            "a fresh ledger mounts the shell again, and the proxy handler answers"
        );
    }

    #[test]
    fn the_mount_ledger_is_a_process_wide_singleton() {
        assert!(std::ptr::eq(MountLedger::shared(), MountLedger::shared()));
        assert!(
            MountLedger::new().claims().is_empty(),
            "a fresh ledger has claimed no prefix"
        );
    }

    /// The proxy shell bounds the request body it buffers with the limit the
    /// data-plane pipeline carries — the `body_limit_bytes` of the same
    /// configuration the pipeline reads its knobs from. A body over it is
    /// refused before any handler runs, a body within it reaches the handler.
    #[tokio::test]
    async fn the_proxy_shell_bounds_the_buffered_body_with_the_pipeline_limit() {
        let stores = InMemoryStores::new();
        let pipeline = pipeline_with_limits(
            &stores,
            Arc::new(StubConnector::new()),
            ProxyLimits {
                body_limit: 8,
                ..ProxyLimits::default()
            },
        );
        assert_eq!(pipeline.body_limit(), 8, "the pipeline carries the limit");
        let router = mount(
            Router::new(),
            &MountLedger::new(),
            Arc::new(ControlPlaneService::default()),
            pipeline,
        )
        .unwrap();

        let over = HttpRequest::builder()
            .method("POST")
            .uri("/oagw/v1/proxy/payments")
            .body(axum::body::Body::from(vec![b'x'; 16]))
            .unwrap();
        let response = router.clone().oneshot(over).await.unwrap();
        assert_eq!(
            response.status(),
            413,
            "a body over the configured limit is refused by the shell, not by the pipeline"
        );

        let within = HttpRequest::builder()
            .method("POST")
            .uri("/oagw/v1/proxy/payments")
            .body(axum::body::Body::from(vec![b'x'; 4]))
            .unwrap();
        let response = router.oneshot(within).await.unwrap();
        assert_eq!(
            response.status(),
            401,
            "a body within the limit reaches the proxy handler"
        );
    }

    /// Mounts the shell over `ledger`, the form the prefix-refusal tests drive.
    fn mounted_over(ledger: &MountLedger) -> Result<Router, OagwError> {
        let control_plane = Arc::new(ControlPlaneService::default());
        mount(
            Router::new(),
            ledger,
            control_plane,
            proxy_pipeline(&InMemoryStores::new(), Arc::new(StubConnector::new())),
        )
    }
}
