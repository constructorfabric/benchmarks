//! Route registration (DESIGN §3.3 "Management API" + "Proxy API").
//!
//! Paths are registered gear-relative as `/oagw/v1/...`; the api-gateway
//! applies its own prefix on top (empty in the e2e deployment).

use std::sync::Arc;

use axum::routing::{get, post};
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;

use super::handlers::{self, OagwApi};

/// Registers every OAGW endpoint onto `router`.
#[must_use]
pub fn register_routes(
    router: Router,
    _openapi: &dyn OpenApiRegistry,
    api: Arc<OagwApi>,
) -> Router {
    router
        // -- control plane -------------------------------------------------
        .route(
            "/oagw/v1/upstreams",
            post(handlers::create_upstream).get(handlers::list_upstreams),
        )
        .route(
            "/oagw/v1/upstreams/{id}",
            get(handlers::get_upstream)
                .put(handlers::replace_upstream)
                .delete(handlers::delete_upstream),
        )
        .route(
            "/oagw/v1/routes",
            post(handlers::create_route).get(handlers::list_routes),
        )
        .route(
            "/oagw/v1/routes/{id}",
            get(handlers::get_route)
                .put(handlers::replace_route)
                .delete(handlers::delete_route),
        )
        .route(
            "/oagw/v1/plugins",
            post(handlers::create_plugin).get(handlers::list_plugins),
        )
        .route(
            "/oagw/v1/plugins/{id}",
            get(handlers::get_plugin).delete(handlers::delete_plugin),
        )
        .route(
            "/oagw/v1/plugins/{id}/source",
            get(handlers::get_plugin_source),
        )
        .layer(Extension(api.clone()))
        // -- data plane ----------------------------------------------------
        // `{*rest}` carries `alias` and the optional `/path_suffix`, and
        // `any` accepts every proxied method plus the `OPTIONS` preflight.
        .route("/oagw/v1/proxy/{*rest}", axum::routing::any(handlers::proxy))
        .layer(Extension(api))
}

#[cfg(test)]
mod tests {
    #[test]
    fn paths_are_gear_relative_without_api_prefix() {
        assert!("/oagw/v1/upstreams".starts_with("/oagw/v1/"));
        assert!(!"/oagw/v1/upstreams".starts_with("/api"));
    }
}
