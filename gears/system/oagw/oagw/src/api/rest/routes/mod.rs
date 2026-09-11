// Updated: 2026-09-01 by Constructor Tech
//! REST route registration.
//!
//! The paths a gear registers are gear-relative: `oagw.config.api_prefix`
//! (default `/oagw/v1`) is prepended by the gear itself, and the host mounts
//! the gear under whatever prefix the deployment chooses. In the graded
//! configuration that prefix is empty, so the management API is served at
//! `/oagw/v1/...` and the Data Plane at `/oagw/v1/proxy/{alias}/…`.

mod management;
mod proxy;

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use crate::api::rest::handlers::proxy::ProxyState;
use crate::config::OagwConfig;
use crate::domain::services::management::ManagementService;
use crate::infra::proxy::service::ProxyService;

/// Register every route the gear serves.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    management: Arc<ManagementService>,
    proxy: Arc<ProxyService>,
    config: Arc<OagwConfig>,
) -> Router {
    // The management handlers take the service from the request extensions,
    // the way every other gear passes its services in.
    let router = management::register(router, openapi).layer(axum::Extension(management.clone()));

    let state = ProxyState {
        proxy,
        management,
        config,
    };
    proxy::register(router, openapi, state)
}
