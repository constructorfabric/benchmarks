//! Tenant scoping — `cpt-cf-oagw-algo-tenant-scope`.
//!
//! The store methods are the predicate: they apply the tenant equality in the
//! same predicate as every other key, so no read or write can address a row
//! the calling tenant does not own. This module exposes only the small
//! decisions the service builds on top of that predicate — which tenant a
//! request carries, what a path-addressed miss answers, and what a body
//! reference the caller does not own answers.
//!
//! No tenant hierarchy is ever walked here: the walk that resolves an
//! ancestor's configuration for a descendant belongs to the hierarchical
//! configuration feature, and the management API's view of an ancestor
//! resource stays empty by construction.

// @cpt-dod:cpt-cf-oagw-dod-tenant-scoping:p1
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::store::{OagwStore, RouteRow, UpstreamRow};
use toolkit_security::SecurityContext;

/// The calling tenant a request carries.
///
/// # Errors
///
/// Returns the catalogue's 401 row when the context carries no tenant, which
/// is the authenticated-subject invariant every request is checked against
/// before any store access.
#[allow(clippy::result_large_err)]
pub fn calling_tenant(context: &SecurityContext) -> Result<Uuid, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-tenant
    let tenant = context.subject_tenant_id();
    if tenant.is_nil() {
        return Err(DomainError::gateway(
            ErrorKind::AuthenticationFailed,
            "the request carries no tenant",
        ));
    }
    Ok(tenant)
    // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-tenant
}

/// The one upstream row the addressed read or write resolves to.
///
/// # Errors
///
/// Returns the 404 row when no row of the calling tenant matches; a
/// nonexistent identifier and a foreign-owned one answer the same way.
#[allow(clippy::result_large_err)]
pub fn resolve_upstream(
    store: &OagwStore,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<UpstreamRow, DomainError> {
    store.get_upstream(tenant_id, id).ok_or_else(path_miss)
}

/// The one route row the addressed read or write resolves to.
///
/// # Errors
///
/// Returns the 404 row when no row of the calling tenant matches; a
/// nonexistent identifier and a foreign-owned one answer the same way.
#[allow(clippy::result_large_err)]
pub fn resolve_route(
    store: &OagwStore,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<RouteRow, DomainError> {
    store.get_route(tenant_id, id).ok_or_else(path_miss)
}

/// Resolves the upstream a route body references on the identifier and the
/// calling tenant.
///
/// # Errors
///
/// Returns a validation error when the reference does not resolve under the
/// calling tenant: a body reference that is not owned by the caller is a
/// validation failure and not a disclosure about another tenant, so it
/// answers 400 and never 404.
#[allow(clippy::result_large_err)]
pub fn resolve_referenced_upstream(
    store: &OagwStore,
    tenant_id: Uuid,
    upstream_id: Uuid,
) -> Result<UpstreamRow, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-upstream-if
    if store.get_upstream(tenant_id, upstream_id).is_none() {
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-upstream
        return Err(DomainError::gateway(
            ErrorKind::ValidationError,
            "upstream_id does not reference an upstream of the calling tenant",
        ));
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-upstream
    }
    // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-upstream-if
    store
        .get_upstream(tenant_id, upstream_id)
        .ok_or_else(path_miss)
}

/// The rows of one tenant a list page is built from.
#[must_use]
pub fn list_upstreams(store: &OagwStore, tenant_id: Uuid) -> Vec<UpstreamRow> {
    store.list_upstreams(tenant_id)
}

/// The route rows of one tenant a list page is built from.
#[must_use]
pub fn list_routes(store: &OagwStore, tenant_id: Uuid) -> Vec<RouteRow> {
    store.list_routes(tenant_id)
}

/// The 404 a path-addressed miss answers with.
///
/// A nonexistent identifier and a foreign-owned one are deliberately
/// indistinguishable: the detail is a constant, the catalogue row is the same,
/// and no row's existence is disclosed. The identifier the request could not
/// parse answers the same row, because an identifier that is not the resource
/// kind's anonymous GTS instance addresses nothing.
#[must_use]
pub fn path_miss() -> DomainError {
    DomainError::gateway(
        ErrorKind::RouteNotFound,
        "the addressed resource does not exist for the calling tenant",
    )
}
