//! The in-memory route store
//! (`cpt-cf-oagw-algo-inmemory-repository`, `cpt-cf-oagw-db-schema`).
//!
//! Routes are keyed by the composite `(tenant_id, id)` and are owned by an
//! upstream of the same tenant, which every write resolves for the
//! referential-integrity rule. Route writes enforce the route-match
//! determinism invariant of `cpt-cf-oagw-db-schema`: no two enabled routes
//! under the same upstream share the same path prefix and priority for the same
//! method.

// @cpt-begin:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full

use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::Route;
use crate::domain::repo::{ResourceLifecycle, RouteRepository, tenant_scope};
use crate::domain::validation::{UpstreamExistence, validate_route};

use super::{Inner, ResourceKind, Stored, already_exists, lookup, not_found};

/// The upstream existence of one tenant, snapshotted for the validation of one
/// candidate, so the referential-integrity rule reads the store without holding
/// a second lock.
#[derive(Debug, Clone)]
struct UpstreamView {
    tenant_id: Uuid,
    upstream_id: Option<Uuid>,
    exists: bool,
    protocol: Option<String>,
}

impl UpstreamExistence for UpstreamView {
    fn upstream_exists(&self, tenant_id: Uuid, upstream_id: Uuid) -> bool {
        self.exists && self.tenant_id == tenant_id && self.upstream_id == Some(upstream_id)
    }

    fn upstream_protocol(&self, tenant_id: Uuid, upstream_id: Uuid) -> Option<String> {
        if self.upstream_exists(tenant_id, upstream_id) {
            self.protocol.clone()
        } else {
            None
        }
    }
}

/// The thread-safe in-memory route store.
#[derive(Debug, Clone)]
pub struct InMemoryRouteRepository {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryRouteRepository {
    pub(crate) fn new(inner: Arc<RwLock<Inner>>) -> Self {
        Self { inner }
    }

    /// Snapshots the upstream a route refers to, the input of the
    /// referential-integrity rule.
    fn upstream_view(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> UpstreamView {
        let inner = self.inner.read();
        let exists = upstream_id
            .is_some_and(|upstream_id| inner.upstreams.contains_key(&(tenant_id, upstream_id)));
        let protocol = upstream_id.and_then(|upstream_id| {
            inner
                .upstreams
                .get(&(tenant_id, upstream_id))
                .and_then(|stored| stored.aggregate.protocol.clone())
        });
        UpstreamView {
            tenant_id,
            upstream_id,
            exists,
            protocol,
        }
    }

    /// Whether `candidate` breaks the route-match determinism invariant against
    /// the enabled routes of the tenant, and the field of the conflict.
    fn determinism_conflict(
        inner: &Inner,
        tenant_id: Uuid,
        candidate: &Route,
    ) -> Option<&'static str> {
        let upstream_id = candidate.upstream_id?;
        if !candidate.enabled {
            return None;
        }
        for ((route_tenant, route_id), stored) in &inner.routes {
            if *route_tenant != tenant_id
                || *route_id == candidate.id.unwrap_or_default()
                || !stored.aggregate.enabled
                || stored.aggregate.upstream_id != Some(upstream_id)
                || stored.aggregate.priority != candidate.priority
            {
                continue;
            }
            let other = &stored.aggregate;
            let candidate_http = candidate
                .match_config
                .as_ref()
                .and_then(|m| m.http.as_ref());
            let other_http = other.match_config.as_ref().and_then(|m| m.http.as_ref());
            if let (Some(candidate_http), Some(other_http)) = (candidate_http, other_http) {
                let shared_method = candidate_http
                    .methods
                    .iter()
                    .any(|method| other_http.methods.contains(method));
                if candidate_http.path == other_http.path && shared_method {
                    return Some("match.http.path");
                }
            }
            let candidate_grpc = candidate
                .match_config
                .as_ref()
                .and_then(|m| m.grpc.as_ref());
            let other_grpc = other.match_config.as_ref().and_then(|m| m.grpc.as_ref());
            if let (Some(candidate_grpc), Some(other_grpc)) = (candidate_grpc, other_grpc)
                && candidate_grpc.service == other_grpc.service
                && candidate_grpc.method == other_grpc.method
            {
                return Some("match.grpc.service");
            }
        }
        None
    }

    // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-02
    /// Stores the candidate, applying the write sequence of every write
    /// operation: validation, the duplicate and missing-key checks, the
    /// determinism invariant, then the mutation.
    fn write(&self, candidate: &Route, replace: bool) -> Result<Route, DomainError> {
        let tenant_id = tenant_scope(candidate.tenant_id)?;
        let id = candidate.id.unwrap_or_default();
        // The referential input of validation is snapshotted before the write
        // lock is taken, so validation never nests a lock.
        let view = self.upstream_view(tenant_id, candidate.upstream_id);
        // The candidate is validated before any mutation is applied.
        validate_route(candidate, &view)?;
        let mut inner = self.inner.write();
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-02
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-04
        if replace {
            // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-08
            if lookup(&inner.routes, tenant_id, id, "route").is_err() {
                // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-08
                // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-09
                return Err(not_found("route"));
                // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-09
            }
            // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-04
        } else if inner.routes.contains_key(&(tenant_id, id))
            || inner
                .deleted
                .contains_key(&(ResourceKind::Route, tenant_id, id))
        {
            return Err(already_exists("id", "identifier"));
        }
        // The upstream must still exist once the write lock is held, so an
        // upstream deleted in between cannot take the reference with it.
        if candidate
            .upstream_id
            .is_some_and(|upstream_id| !inner.upstreams.contains_key(&(tenant_id, upstream_id)))
        {
            return Err(not_found("upstream"));
        }
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-10
        // The route-match determinism invariant: no two enabled routes under
        // the same upstream share the same path prefix and priority for the
        // same method.
        if let Some(field) = Self::determinism_conflict(&inner, tenant_id, candidate) {
            return Err(already_exists(field, "route match key"));
        }
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-10
        // The composite write is one atomic step.
        let lifecycle = ResourceLifecycle::from_enabled(candidate.enabled);
        let seq = inner.next_seq();
        inner.routes.insert(
            (tenant_id, id),
            Stored::new(candidate.clone(), lifecycle, seq),
        );
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-11
        // The stored view is returned to the caller.
        Ok(candidate.clone())
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-11
    }
}

impl RouteRepository for InMemoryRouteRepository {
    fn insert(&self, route: &Route) -> Result<Route, DomainError> {
        self.write(route, false)
    }

    fn replace(&self, route: &Route) -> Result<Route, DomainError> {
        self.write(route, true)
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        let inner = self.inner.read();
        lookup(&inner.routes, tenant_id, id, "route").map(|stored| stored.aggregate.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        let inner = self.inner.read();
        let mut stored: Vec<&Stored<Route>> = inner
            .routes
            .iter()
            .filter(|((route_tenant, _), _)| *route_tenant == tenant_id)
            .map(|(_, stored)| stored)
            .collect();
        stored.sort_by_key(|entry| entry.seq);
        Ok(stored
            .into_iter()
            .map(|entry| entry.aggregate.clone())
            .collect())
    }

    fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        let inner = self.inner.read();
        if lookup(&inner.upstreams, tenant_id, upstream_id, "upstream").is_err() {
            return Err(not_found("upstream"));
        }
        // The deterministic order the route-match invariant needs: priority
        // descending, then the identifier.
        let mut routes: Vec<Route> = inner
            .routes
            .iter()
            .filter(|((route_tenant, _), stored)| {
                *route_tenant == tenant_id && stored.aggregate.upstream_id == Some(upstream_id)
            })
            .map(|(_, stored)| stored.aggregate.clone())
            .collect();
        routes.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(routes)
    }

    fn set_enabled(&self, tenant_id: Uuid, id: Uuid, enabled: bool) -> Result<Route, DomainError> {
        let candidate = {
            let inner = self.inner.read();
            lookup(&inner.routes, tenant_id, id, "route")
                .map(|stored| {
                    let mut candidate = stored.aggregate.clone();
                    candidate.enabled = enabled;
                    candidate
                })
                .map_err(|_| not_found("route"))?
        };
        validate_route(
            &candidate,
            &self.upstream_view(tenant_id, candidate.upstream_id),
        )?;
        let mut inner = self.inner.write();
        if let Some(field) = Self::determinism_conflict(&inner, tenant_id, &candidate) {
            return Err(already_exists(field, "route match key"));
        }
        let stored = inner.routes.get_mut(&(tenant_id, id)).expect("present");
        stored.lifecycle = stored
            .lifecycle
            .transition(ResourceLifecycle::from_enabled(enabled))
            .ok_or_else(|| not_found("route"))?;
        stored.aggregate = candidate.clone();
        Ok(candidate)
    }

    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError> {
        let inner = self.inner.read();
        if let Some(lifecycle) = inner.deleted.get(&(ResourceKind::Route, tenant_id, id)) {
            return Ok(*lifecycle);
        }
        lookup(&inner.routes, tenant_id, id, "route").map(|stored| stored.lifecycle)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        let mut inner = self.inner.write();
        if lookup(&inner.routes, tenant_id, id, "route").is_err() {
            return Err(not_found("route"));
        }
        // The plugin bindings of the route go with the route itself, applied
        // under the same lock as the delete.
        let removed = inner.routes.remove(&(tenant_id, id));
        inner.deleted.insert(
            (ResourceKind::Route, tenant_id, id),
            ResourceLifecycle::Deleted,
        );
        removed
            .map(|stored| stored.aggregate)
            .ok_or_else(|| not_found("route"))
    }
}

// @cpt-end:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full
