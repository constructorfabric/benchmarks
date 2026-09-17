//! [`RouteRepository`] contract (mirrors `oagw_route`, `oagw_route_http_match`,
//! `oagw_route_grpc_match`, `oagw_route_method`).

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::entity::{HttpMatch, Route, RouteMatch};
use crate::domain::repo::RepoResult;

/// Repository for tenant-scoped `Route` entities.
///
/// Routes belong to exactly one upstream (`upstream_id` immutable); the
/// repository enforces the match-rule invariant that no two enabled routes
/// under the same upstream share `(path_prefix, priority)` for the same
/// method (DoD `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`).
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Persists a new route after validating its match shape and the
    /// match-rule uniqueness invariant.
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::Validation`] when the match is
    ///   malformed or an enabled route already claims
    ///   `(path_prefix, priority)` for one of the same methods under the same
    ///   upstream.
    async fn create(&self, tenant_id: Uuid, route: Route) -> RepoResult<Route>;

    /// Fetches a route by `(tenant_id, id)`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;

    /// Lists all routes owned by the tenant.
    async fn list(&self, tenant_id: Uuid) -> Vec<Route>;

    /// Lists the routes bound to `(tenant_id, upstream_id)`.
    async fn list_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route>;

    /// Replaces the stored route with `route` (same `id`); `upstream_id` is
    /// immutable and must match the stored row.
    ///
    /// Returns `None` when no row with `(tenant_id, id)` exists.
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::Validation`] when `upstream_id`
    ///   differs from the stored row or the match-rule invariant is violated.
    async fn update(&self, tenant_id: Uuid, route: Route) -> RepoResult<Option<Route>>;

    /// Deletes a route by `(tenant_id, id)` (cascades to its match/method
    /// rows).
    ///
    /// Returns `false` when no row was present.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool>;
}

/// Extracts the HTTP match payload from a route, when it is an HTTP route.
///
/// # Errors
/// Returns a descriptive validation error when the route carries a gRPC match
/// shape or an incomplete HTTP match.
pub fn http_match_of(route: &Route) -> Result<&HttpMatch, String> {
    match &route.match_ {
        RouteMatch::Http(m) => {
            if m.path_prefix.trim().is_empty() {
                return Err("http match requires a non-empty path_prefix".to_owned());
            }
            Ok(m)
        }
        RouteMatch::Grpc(_) => {
            Err("gRPC match shapes are reserved (Phase 3) and not reachable".to_owned())
        }
    }
}
