//! REST surface of the OAGW gear.
//!
//! All routes are registered unprefixed through
//! [`RestApiCapability::register_rest`](toolkit::contracts::RestApiCapability);
//! the platform api-gateway applies the `/api/oagw/v1/...` prefix (DoD
//! `cpt-cf-oagw-dod-gear-foundation-rest-openapi`, algorithm
//! `cpt-cf-oagw-algo-gear-foundation-register-surface`, flow
//! `cpt-cf-oagw-flow-gear-foundation-boot`).
//!
//! Plane routing by path prefix (DoD
//! `cpt-cf-oagw-dod-gear-foundation-plane-routing`, flow
//! `cpt-cf-oagw-flow-gear-foundation-plane-routing`):
//! - `/upstreams`, `/routes`, `/plugins` → Control Plane boundary;
//! - `/proxy` → Data Plane boundary;
//! - anything else → not-found outcome.
//!
//! Handler registration is populated by the Control-Plane Management API and
//! Data-Plane Proxy features (phases p3/p5); the Control-Plane Management API
//! (feature `cpt-cf-oagw-feature-control-plane-api`) registers its operations
//! in [`routes::register_routes`] and attaches the shared [`GearState`]
//! Extension.

mod dto;
mod handlers;
mod metrics;
mod proxy;
mod routes;
#[cfg(test)]
pub mod tests_api;

use std::sync::Arc;

use toolkit::api::OpenApiRegistry;

use crate::domain::GearState;

/// Registers the OAGW REST surface on the given axum router (flow
/// `cpt-cf-oagw-flow-gear-foundation-boot`, step `inst-gf-boot-rest`).
///
/// Delegates to [`routes::register_routes`], which registers the control-plane
/// operations (feature `cpt-cf-oagw-feature-control-plane-api`) and attaches
/// the shared [`GearState`] as an `Extension` layer so every handler can
/// access the service boundaries without re-plumbing.  The Data-Plane Proxy
/// feature (phase p5) registers `/proxy` here.
///
/// # Errors
/// Returns an error if route/schema registration fails (duplicate path or
/// schema conflict).
pub fn register_routes(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<GearState>,
) -> anyhow::Result<axum::Router> {
    Ok(routes::register_routes(router, openapi, state))
}

/// Dispatches an inbound path to its plane boundary.
///
/// Returns `Control`/`Data` classification per the path prefixes above, or
/// `None` for an unmatched prefix (not-found outcome).
#[must_use]
pub fn classify_plane(path: &str) -> Option<Plane> {
    let trimmed = path.trim_start_matches('/');
    let (first, _) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    match first {
        "upstreams" | "routes" | "plugins" => Some(Plane::Control),
        "proxy" => Some(Plane::Data),
        _ => None,
    }
}

/// Service boundary a request is dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    /// Management CRUD for upstreams, routes, plugins.
    Control,
    /// Proxy hot path.
    Data,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_plane_paths() {
        assert_eq!(classify_plane("/upstreams"), Some(Plane::Control));
        assert_eq!(classify_plane("/upstreams/abc"), Some(Plane::Control));
        assert_eq!(classify_plane("/routes"), Some(Plane::Control));
        assert_eq!(classify_plane("/plugins/9"), Some(Plane::Control));
    }

    #[test]
    fn data_plane_paths() {
        assert_eq!(classify_plane("/proxy"), Some(Plane::Data));
        assert_eq!(classify_plane("/proxy/alias/path?q=1"), Some(Plane::Data));
    }

    #[test]
    fn unmatched_prefix_is_not_found() {
        assert_eq!(classify_plane("/other"), None);
        assert_eq!(classify_plane("/"), None);
    }
}
