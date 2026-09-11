//! The in-memory upstream store
//! (`cpt-cf-oagw-algo-inmemory-repository`, `cpt-cf-oagw-db-schema`).
//!
//! Upstreams are keyed by the composite `(tenant_id, id)` and indexed by
//! `(tenant_id, alias)`, so a second upstream with the same alias in the same
//! tenant is a duplicate while the same alias in another tenant is a different
//! aggregate. Deleting one cascades to its routes.

// @cpt-begin:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full

use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::Upstream;
use crate::domain::repo::{ResourceLifecycle, UpstreamRepository, tenant_scope};
use crate::domain::validation::validate_upstream;

use super::{Inner, ResourceKind, Stored, already_exists, lookup, not_found};

/// The thread-safe in-memory upstream store.
///
/// All state is behind one shared [`parking_lot::RwLock`], so a write and its
/// index update are atomic and the store is usable from several threads.
#[derive(Debug, Clone)]
pub struct InMemoryUpstreamRepository {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryUpstreamRepository {
    pub(crate) fn new(inner: Arc<RwLock<Inner>>) -> Self {
        Self { inner }
    }
}

impl UpstreamRepository for InMemoryUpstreamRepository {
    fn insert(&self, upstream: &Upstream) -> Result<Upstream, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-03
        // The candidate is validated before any mutation is applied, so a
        // rejected payload leaves the store untouched; validation also rejects
        // an aggregate that carries no tenant context.
        validate_upstream(upstream)?;
        let tenant_id = tenant_scope(upstream.tenant_id)?;
        // A candidate that carries no identifier is stored under the one the
        // server mints for it here, so the stored aggregate always carries an
        // identity — the resolver's route tiers, the delete cascade and the
        // management surface all read it — and is never keyed by the nil UUID,
        // which no read of the store can reach a resource through.
        let id = upstream.id.unwrap_or_else(Uuid::new_v4);
        let alias = upstream.alias.clone();
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-03
        let mut inner = self.inner.write();
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-06
        let taken_id = inner.upstreams.contains_key(&(tenant_id, id))
            || inner
                .deleted
                .contains_key(&(ResourceKind::Upstream, tenant_id, id));
        let taken_alias = alias.as_ref().is_some_and(|alias| {
            inner
                .upstream_aliases
                .contains_key(&(tenant_id, alias.clone()))
        });
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-06
        if taken_id || taken_alias {
            // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-07
            return Err(already_exists(
                if taken_id { "id" } else { "alias" },
                "identifier or alias",
            ));
            // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-07
        }
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-01
        // The store is keyed by the composite `(tenant_id, id)` of the
        // aggregate and, for upstreams, additionally by
        // `(tenant_id, alias)`. The composite write and its index update are
        // one atomic step under the same lock.
        let lifecycle = ResourceLifecycle::from_enabled(upstream.enabled);
        let seq = inner.next_seq();
        // The stored view carries the identifier the store resolved, minted or
        // caller-supplied alike, and that is the view the caller reads back.
        let mut stored = upstream.clone();
        stored.id = Some(id);
        inner
            .upstreams
            .insert((tenant_id, id), Stored::new(stored.clone(), lifecycle, seq));
        if let Some(alias) = alias {
            inner.upstream_aliases.insert((tenant_id, alias), id);
        }
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-01
        Ok(stored)
    }

    fn replace(&self, upstream: &Upstream) -> Result<Upstream, DomainError> {
        // The candidate is validated before any mutation is applied.
        validate_upstream(upstream)?;
        let tenant_id = tenant_scope(upstream.tenant_id)?;
        // A replacement names the aggregate it replaces, so a candidate that
        // carries no identifier resolves to the nil key — a key no insert ever
        // stores under — and the lookup below refuses it as not-found rather
        // than writing an aggregate the composite reads cannot reach.
        let id = upstream.id.unwrap_or_default();
        let alias = upstream.alias.clone();
        let mut inner = self.inner.write();
        if lookup(&inner.upstreams, tenant_id, id, "upstream").is_err() {
            return Err(not_found("upstream"));
        }
        // A taken alias of a sibling upstream is a duplicate.
        if let Some(alias) = &alias
            && let Some(holder) = inner.upstream_aliases.get(&(tenant_id, alias.clone()))
            && *holder != id
        {
            return Err(already_exists("alias", "alias"));
        }
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-05
        // Replacement is wholesale, so the optional fields the candidate omits
        // are cleared with it, and the composite write and its index update are
        // one atomic step.
        if let Some(removed) = inner.upstreams.remove(&(tenant_id, id))
            && let Some(stale) = removed.aggregate.alias
            && Some(&stale) != alias.as_ref()
        {
            inner.upstream_aliases.remove(&(tenant_id, stale));
        }
        let lifecycle = ResourceLifecycle::from_enabled(upstream.enabled);
        let seq = inner.next_seq();
        inner.upstreams.insert(
            (tenant_id, id),
            Stored::new(upstream.clone(), lifecycle, seq),
        );
        if let Some(alias) = alias {
            inner.upstream_aliases.insert((tenant_id, alias), id);
        }
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-05
        Ok(upstream.clone())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let inner = self.inner.read();
        lookup(&inner.upstreams, tenant_id, id, "upstream").map(|stored| stored.aggregate.clone())
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        let inner = self.inner.read();
        let Some(id) = inner.upstream_aliases.get(&(tenant_id, alias.to_owned())) else {
            return Err(DomainError::not_found(
                "alias",
                format!("no upstream with the alias '{alias}' exists in the caller's tenant"),
            ));
        };
        lookup(&inner.upstreams, tenant_id, *id, "upstream").map(|stored| stored.aggregate.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let inner = self.inner.read();
        let mut stored: Vec<&Stored<Upstream>> = inner
            .upstreams
            .iter()
            .filter(|((tenant, _), _)| *tenant == tenant_id)
            .map(|(_, stored)| stored)
            .collect();
        stored.sort_by_key(|entry| entry.seq);
        Ok(stored
            .into_iter()
            .map(|entry| entry.aggregate.clone())
            .collect())
    }

    fn set_enabled(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError> {
        let mut inner = self.inner.write();
        if lookup(&inner.upstreams, tenant_id, id, "upstream").is_err() {
            return Err(not_found("upstream"));
        }
        let stored = inner.upstreams.get_mut(&(tenant_id, id)).expect("present");
        let mut candidate = stored.aggregate.clone();
        candidate.enabled = enabled;
        validate_upstream(&candidate)?;
        // The `Active`/`Disabled` transition is refused only for a resource that
        // left the lifecycle, which a stored upstream cannot have done.
        stored.lifecycle = stored
            .lifecycle
            .transition(ResourceLifecycle::from_enabled(enabled))
            .ok_or_else(|| not_found("upstream"))?;
        stored.aggregate = candidate.clone();
        Ok(candidate)
    }

    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError> {
        let inner = self.inner.read();
        if let Some(lifecycle) = inner.deleted.get(&(ResourceKind::Upstream, tenant_id, id)) {
            return Ok(*lifecycle);
        }
        lookup(&inner.upstreams, tenant_id, id, "upstream").map(|stored| stored.lifecycle)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let mut inner = self.inner.write();
        if lookup(&inner.upstreams, tenant_id, id, "upstream").is_err() {
            return Err(not_found("upstream"));
        }
        // The delete cascade of `cpt-cf-oagw-db-schema`: removing an upstream
        // removes its routes, and the plugin bindings of both go with them. The
        // cascade is applied under the same lock as the delete, so no caller
        // observes a half-cascaded store.
        let cascaded: Vec<Uuid> = inner
            .routes
            .iter()
            .filter(|((route_tenant, _), stored)| {
                *route_tenant == tenant_id && stored.aggregate.upstream_id == Some(id)
            })
            .map(|((_, route_id), _)| *route_id)
            .collect();
        for route_id in cascaded {
            inner.routes.remove(&(tenant_id, route_id));
            inner.deleted.insert(
                (ResourceKind::Route, tenant_id, route_id),
                ResourceLifecycle::Deleted,
            );
        }
        let removed = inner.upstreams.remove(&(tenant_id, id));
        inner
            .upstream_aliases
            .retain(|alias_key, holder| !(alias_key.0 == tenant_id && *holder == id));
        inner.deleted.insert(
            (ResourceKind::Upstream, tenant_id, id),
            ResourceLifecycle::Deleted,
        );
        removed
            .map(|stored| stored.aggregate)
            .ok_or_else(|| not_found("upstream"))
    }
}

// @cpt-end:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full
