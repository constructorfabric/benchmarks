//! REST handlers for the OAGW gear.
//!
//! Handlers consume service handles via axum `Extension` (wired in
//! `routes.rs`) and a platform-injected [`SecurityContext`]. Management
//! handlers are strictly tenant-scoped via `subject_tenant_id()`; the proxy
//! handler resolves the target against the same tenant.
//!
//! [`SecurityContext`]: toolkit_security::SecurityContext

pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;

use axum::http::StatusCode;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::services::data_plane::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};

use super::error::OagwProblem;

/// Convert a domain error into a problem response, tagging the source as
/// gateway (ADR-0007) and binding `instance` to the request path.
#[must_use]
pub fn problem(err: &DomainError, instance: &str) -> OagwProblem {
    OagwProblem::from_domain_error(err, Some(instance.to_owned()))
}

/// The tenant all management/proxy operations are scoped to.
#[must_use]
pub fn tenant_of(ctx: &SecurityContext) -> uuid::Uuid {
    ctx.subject_tenant_id()
}

/// Marker header insertion used by problem responses (provided here for
/// handler-level reuse when embedding non-problem responses).
pub const PROBLEM_SOURCE_HEADER: &str = ERROR_SOURCE_HEADER;
pub const PROBLEM_SOURCE_GATEWAY: &str = ERROR_SOURCE_GATEWAY;

/// Build a 204 No Content response (delete/completion endpoints).
#[must_use]
pub fn no_content() -> axum::response::Response {
    let mut response = axum::response::Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

/// Convenience alias so handlers read `Result<T, OagwProblem>` uniformly.
pub type HandlerResult<T> = Result<T, OagwProblem>;
