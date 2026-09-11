//! REST surface of the `oagw` gear.

pub mod dto;
pub mod handlers;
pub mod params;
pub mod problem;
pub mod state;

use std::sync::Arc;

use axum::Router;

use crate::api::rest::state::OagwState;

/// The mount point every OAGW REST route hangs under.
pub const MOUNT_POINT: &str = "/oagw/v1";

/// Nests the OAGW mount point under the runtime router.
///
/// The foundation feature adds no route to it: every `/oagw/v1/**` path
/// answers 404 until a feature registers handlers.
#[must_use = "the mounted router must be returned to the runtime"]
pub fn nest_mount_point(parent: Router) -> Router {
    parent.nest(MOUNT_POINT, Router::new())
}

/// Registers the ten management endpoints on the mount point.
///
/// Exactly the ten paths DECOMPOSITION §2.2 assigns to the management feature
/// are registered, and the five plugin paths of DESIGN §3.3 the plugin system
/// feature adds to the same mount point.
///
/// The `{id}` path parameter is the resource's anonymous GTS instance, and the
/// list endpoints read their five OData parameters from the query string.
#[must_use = "the mounted router must be returned to the runtime"]
pub fn register_management_routes(parent: Router, state: Arc<OagwState>) -> Router {
    // @cpt-begin:cpt-cf-oagw-dod-management-routes:p1:inst-mgmt-routes
    let management = Router::new()
        .route(
            "/upstreams",
            axum::routing::post(handlers::upstreams::create).get(handlers::upstreams::list),
        )
        .route(
            "/upstreams/{id}",
            axum::routing::get(handlers::upstreams::read)
                .put(handlers::upstreams::replace)
                .delete(handlers::upstreams::delete),
        )
        .route(
            "/routes",
            axum::routing::post(handlers::routes::create).get(handlers::routes::list),
        )
        .route(
            "/routes/{id}",
            axum::routing::get(handlers::routes::read)
                .put(handlers::routes::replace)
                .delete(handlers::routes::delete),
        )
        // @cpt-begin:cpt-cf-oagw-dod-plugin-management-api:p1:inst-pl-routes
        // The five plugin endpoints of DESIGN §3.3: the two management features
        // share one mount point, so the plugin routes join the same router
        // rather than a second one, and no replacement route is registered —
        // plugins are immutable after creation.
        .route(
            "/plugins",
            axum::routing::post(handlers::plugins::create).get(handlers::plugins::list),
        )
        .route(
            "/plugins/{id}",
            axum::routing::get(handlers::plugins::read).delete(handlers::plugins::delete),
        )
        .route(
            "/plugins/{id}/source",
            axum::routing::get(handlers::plugins::source),
        )
        // @cpt-end:cpt-cf-oagw-dod-plugin-management-api:p1:inst-pl-routes
        // @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint:p1:inst-px-routes
        // The two proxy paths: every method is admitted to the handler, whose
        // match step answers the methods no route declares, so no method
        // routing sits between the platform middleware and the authorize step.
        .route(
            "/proxy/{alias}",
            axum::routing::any(handlers::proxy::root),
        )
        // @cpt-begin:cpt-cf-oagw-dod-obs-metrics:p1:inst-ms-routes
        // The one path this feature registers, for that method alone.
        .route("/metrics", axum::routing::get(handlers::metrics::scrape))
        // @cpt-end:cpt-cf-oagw-dod-obs-metrics:p1:inst-ms-routes
        .route(
            "/proxy/{alias}/{*path_suffix}",
            axum::routing::any(handlers::proxy::suffix),
        )
        // @cpt-end:cpt-cf-oagw-dod-proxy-endpoint:p1:inst-px-routes
        .with_state(state);
    // @cpt-end:cpt-cf-oagw-dod-management-routes:p1:inst-mgmt-routes

    parent.nest(MOUNT_POINT, management)
}
