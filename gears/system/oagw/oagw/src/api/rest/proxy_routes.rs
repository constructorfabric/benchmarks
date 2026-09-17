//! REST route registration of the proxy data plane.
//!
//! Fourteen registrations, one per method on each of the two proxy paths: the
//! alias alone and the alias plus a suffix path. They all funnel into the one
//! handler [`proxy_request`], which is why the five documented verbs share a
//! single [`proxy_responses`] body through the [`proxy_method!`] macro.
//!
//! `HEAD` and `OPTIONS` are registered through plain axum routing rather than
//! [`OperationBuilder`]: its [`OperationBuilder::handler`] maps an undocumented
//! verb to an `any(405)` method router, and a `MethodRouter` carrying a
//! fallback cannot be merged with the ones the documented verbs build for the
//! same path. They stay out of the OpenAPI document, which is where they belong:
//! both are always available on a proxied path.
//!
//! Paths are **gear-relative** (`/oagw/v1/proxy/{alias}`), like every other
//! route of the gear. The one response is `*/*`: a proxied call returns
//! whatever the upstream returned, including a `text/event-stream`.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::MethodFilter;
use toolkit::api::operation_builder::HandlerSlot;
use toolkit::api::operation_builder::{AuthSet, LicenseSet, Missing, Present};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ResponseSpec};

use super::proxy::{PROXY_ALIAS_PATH, PROXY_SUFFIX_PATH, ProxyState, proxy_request};

/// The single OpenAPI tag of every OAGW operation.
const TAG: &str = "oagw";

/// Registers one method of one proxy path. The typestate of
/// [`OperationBuilder`] is inferred at each expansion, so no state type has to
/// be named here.
macro_rules! proxy_method {
    ($router:expr, $openapi:expr, head, $path:expr) => {{
        proxy_method!(@ $router, $openapi, OperationBuilder::new(Method::HEAD, $path))
    }};
    ($router:expr, $openapi:expr, options, $path:expr) => {{
        proxy_method!(@ $router, $openapi, OperationBuilder::new(Method::OPTIONS, $path))
    }};
    ($router:expr, $openapi:expr, $method:ident, $path:expr) => {{
        proxy_method!(@ $router, $openapi, OperationBuilder::$method($path))
    }};
    (@ $router:expr, $openapi:expr, $builder:expr) => {{
        let builder = $builder
            .operation_id("oagw.proxy")
            .summary("Proxy a request")
            .description(
                "Forward the request to the upstream its alias resolves to, streaming both \
                 bodies. The path suffix after the alias is appended to the endpoint path.",
            )
            .tag(TAG)
            .authenticated()
            .no_license_required()
            .handler(proxy_request);
        proxy_responses(builder, $openapi).register($router, $openapi)
    }};
}

/// Registers every proxy route of the gear onto `router`, layered with the
/// shared [`ProxyState`] extension.
///
/// The five documented verbs go through [`OperationBuilder`]; `HEAD` and
/// `OPTIONS` go through plain axum routing (see the module documentation).
pub fn register_proxy_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<ProxyState>,
) -> Router {
    let router = proxy_method!(router, openapi, get, PROXY_ALIAS_PATH);
    let router = proxy_method!(router, openapi, post, PROXY_ALIAS_PATH);
    let router = proxy_method!(router, openapi, put, PROXY_ALIAS_PATH);
    let router = proxy_method!(router, openapi, patch, PROXY_ALIAS_PATH);
    let router = proxy_method!(router, openapi, delete, PROXY_ALIAS_PATH);
    let router = proxy_method!(router, openapi, get, PROXY_SUFFIX_PATH);
    let router = proxy_method!(router, openapi, post, PROXY_SUFFIX_PATH);
    let router = proxy_method!(router, openapi, put, PROXY_SUFFIX_PATH);
    let router = proxy_method!(router, openapi, patch, PROXY_SUFFIX_PATH);
    let router = proxy_method!(router, openapi, delete, PROXY_SUFFIX_PATH);
    let router = router.route(
        PROXY_ALIAS_PATH,
        axum::routing::on(MethodFilter::HEAD, proxy_request),
    );
    let router = router.route(PROXY_ALIAS_PATH, axum::routing::options(proxy_request));
    let router = router.route(
        PROXY_SUFFIX_PATH,
        axum::routing::on(MethodFilter::HEAD, proxy_request),
    );
    router
        .route(PROXY_SUFFIX_PATH, axum::routing::options(proxy_request))
        .layer(axum::Extension(state))
}

/// The response set of a proxy operation: the passthrough response of any
/// content type, plus the problem responses the data plane can return.
///
/// `ResponseSpec { schema: None }` is the OpenAPI way of saying "any body",
/// which is what a proxy returns.
fn proxy_responses<H, S>(
    builder: OperationBuilder<H, Missing, S, AuthSet, LicenseSet>,
    openapi: &dyn OpenApiRegistry,
) -> OperationBuilder<H, Present, S, AuthSet, LicenseSet>
where
    H: HandlerSlot<S>,
{
    builder
        .response(ResponseSpec {
            status: StatusCode::OK.as_u16(),
            content_type: "*/*",
            description: "The upstream's response, passed through".to_owned(),
            schema: None,
        })
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "A routing, target-host or framing error",
        )
        .problem_response(
            openapi,
            StatusCode::UNAUTHORIZED,
            "The caller is not authenticated",
        )
        .problem_response(openapi, StatusCode::NOT_FOUND, "The alias is unknown")
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "The body exceeds the proxy limit",
        )
        .problem_response(
            openapi,
            StatusCode::SERVICE_UNAVAILABLE,
            "The upstream or route is disabled",
        )
        .problem_response(
            openapi,
            StatusCode::GATEWAY_TIMEOUT,
            "The upstream did not answer in time",
        )
}
