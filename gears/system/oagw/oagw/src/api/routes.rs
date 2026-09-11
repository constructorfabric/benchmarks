//! Route registration for both surface families.
//!
//! The management API and the data plane are mounted at `/oagw/v1/...` — the
//! gear-relative form, with no leading `/api`.
//!
//! The routes are registered as plain axum routes rather than through
//! [`toolkit::api::operation_builder::OperationBuilder`]: the proxy endpoint is
//! method-agnostic (`{METHOD} /oagw/v1/proxy/{alias}`) and streams both its
//! request and its response body, which an OpenAPI operation spec has no way to
//! say. The platform middleware still authenticates every one of these routes
//! — the deployment sets `require_auth_by_default: true`, which is the
//! fallback for a path with no operation spec.

use axum::Extension;
use axum::Router;
use axum::routing::{get, post};
use toolkit::api::OpenApiRegistry;

use super::{ApiState, handlers, proxy};

/// The path every proxy method is mounted at, without a suffix.
const PROXY_ALIAS: &str = "/oagw/v1/proxy/{alias}";
/// The proxy with a path suffix appended to the matched route's path.
const PROXY_ALIAS_PATH: &str = "/oagw/v1/proxy/{alias}/{*path_suffix}";

const UPSTREAMS: &str = "/oagw/v1/upstreams";
const UPSTREAM: &str = "/oagw/v1/upstreams/{id}";
const ROUTES: &str = "/oagw/v1/routes";
const ROUTE: &str = "/oagw/v1/routes/{id}";
const PLUGINS: &str = "/oagw/v1/plugins";
const PLUGIN: &str = "/oagw/v1/plugins/{id}";
const PLUGIN_SOURCE: &str = "/oagw/v1/plugins/{id}/source";

/// Wire every `oagw` route into the host router.
pub fn register_routes(router: Router, _openapi: &dyn OpenApiRegistry, state: ApiState) -> Router {
    router
        .route(
            UPSTREAMS,
            post(handlers::create_upstream).get(handlers::list_upstreams),
        )
        .route(
            UPSTREAM,
            get(handlers::get_upstream)
                .put(handlers::replace_upstream)
                .delete(handlers::delete_upstream),
        )
        .route(
            ROUTES,
            post(handlers::create_route).get(handlers::list_routes),
        )
        .route(
            ROUTE,
            get(handlers::get_route)
                .put(handlers::replace_route)
                .delete(handlers::delete_route),
        )
        .route(
            PLUGINS,
            post(handlers::create_plugin).get(handlers::list_plugins),
        )
        .route(
            PLUGIN,
            get(handlers::get_plugin).delete(handlers::delete_plugin),
        )
        .route(PLUGIN_SOURCE, get(handlers::get_plugin_source))
        // `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` — one handler for
        // every verb, including OPTIONS, which the handler answers itself when
        // it is a CORS preflight (ADR 0004).
        .route(
            PROXY_ALIAS,
            get(proxy::proxy)
                .post(proxy::proxy)
                .put(proxy::proxy)
                .patch(proxy::proxy)
                .delete(proxy::proxy)
                .head(proxy::proxy)
                .options(proxy::proxy),
        )
        .route(
            PROXY_ALIAS_PATH,
            get(proxy::proxy)
                .post(proxy::proxy)
                .put(proxy::proxy)
                .patch(proxy::proxy)
                .delete(proxy::proxy)
                .head(proxy::proxy)
                .options(proxy::proxy),
        )
        .layer(Extension(state))
}
