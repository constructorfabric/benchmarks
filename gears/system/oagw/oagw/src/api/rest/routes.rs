//! REST route aggregator for the `oagw` gear.
//!
//! Mounts every management/proxy submodule's routes onto the shared router
//! in a fixed order, so entries 2.2-2.6 each only ever edit their own file
//! and never this one:
//!
//! 1. [`super::upstreams`] -- Upstream Management API (2.2)
//! 2. [`super::route_api`] -- Route Management API (2.3)
//! 3. [`super::plugins`] -- Plugin Management API (2.4)
//! 4. [`super::proxy`] -- Proxy Request Resolution/Forwarding and
//!    Streaming (2.5/2.6)

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use crate::store::OagwState;

/// Registers every REST route the `oagw` gear owns, under `/oagw/v1/...`.
pub(crate) fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = super::upstreams::register_routes(router, openapi, state.clone());
    let router = super::route_api::register_routes(router, openapi, state.clone());
    let router = super::plugins::register_routes(router, openapi, state.clone());
    super::proxy::register_routes(router, openapi, state)
}
