//! DashMap-backed [`RouteRepository`] implementation.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::entity::Route;
use crate::domain::error::DomainError;
use crate::domain::repo::{RepoResult, RouteRepository, http_match_of};

use super::StoreTables;

/// In-memory `oagw_route` implementation.
pub struct InMemoryRouteRepo {
    tables: Arc<StoreTables>,
    lock: Arc<Mutex<()>>,
}

impl InMemoryRouteRepo {
    #[must_use]
    pub(crate) fn new(tables: Arc<StoreTables>, lock: Arc<Mutex<()>>) -> Self {
        Self { tables, lock }
    }

    /// Validates the route match shape and the match-rule uniqueness
    /// invariant under the given lock.
    fn validate(&self, tenant_id: Uuid, route: &Route) -> Result<(), DomainError> {
        let m = http_match_of(route).map_err(|e| DomainError::validation(None, e))?;

        // No two enabled routes under the same upstream share
        // `(path_prefix, priority)` for the same method.
        if route.enabled {
            let siblings = self
                .tables
                .routes
                .iter()
                .filter(|r| {
                    r.key().0 == tenant_id
                        && r.value().upstream_id == route.upstream_id
                        && r.value().id != route.id
                        && r.value().enabled
                })
                .map(|r| r.value().clone())
                .collect::<Vec<_>>();

            for sibling in siblings {
                if sibling.priority != route.priority {
                    continue;
                }
                let Ok(sm) = http_match_of(&sibling) else {
                    continue;
                };
                if sm.path_prefix != m.path_prefix {
                    continue;
                }
                let clash = sm.methods.iter().any(|m1| m.methods.contains(m1));
                if clash {
                    return Err(DomainError::validation(
                        None,
                        format!(
                            "route conflicts with route '{}': same path_prefix '{}', \
                             priority {}, and at least one shared method, under the \
                             same upstream",
                            sibling.id, m.path_prefix, route.priority
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RouteRepository for InMemoryRouteRepo {
    async fn create(&self, tenant_id: Uuid, route: Route) -> RepoResult<Route> {
        let _guard = self.lock.lock();
        self.validate(tenant_id, &route)?;

        let mut stored = route;
        let now = SystemTime::now();
        stored.created_at = Some(now);
        stored.updated_at = Some(now);

        let key = (tenant_id, stored.id);
        let idx_key = (tenant_id, stored.upstream_id);
        self.tables
            .route_upstream
            .entry(idx_key)
            .or_default()
            .push(stored.id);
        self.tables.routes.insert(key, stored.clone());
        Ok(stored)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.tables.routes.get(&(tenant_id, id)).map(|r| r.clone())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut rows: Vec<Route> = self
            .tables
            .routes
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        rows.sort_by_key(|r| r.id);
        rows
    }

    async fn list_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route> {
        let mut rows: Vec<Route> = self
            .tables
            .routes
            .iter()
            .filter(|r| r.key().0 == tenant_id && r.value().upstream_id == upstream_id)
            .map(|r| r.value().clone())
            .collect();
        rows.sort_by_key(|r| r.id);
        rows
    }

    async fn update(&self, tenant_id: Uuid, route: Route) -> RepoResult<Option<Route>> {
        let _guard = self.lock.lock();
        let key = (tenant_id, route.id);
        // Clone out of the DashMap read guard: the `Ref` would otherwise hold
        // the shard read lock while `insert` below needs the write lock,
        // deadlocking on the same thread.
        let Some(existing) = self.tables.routes.get(&key).map(|r| r.value().clone()) else {
            return Ok(None);
        };

        // `upstream_id` is immutable via the API (mirrors §3.7).
        if existing.upstream_id != route.upstream_id {
            return Err(DomainError::validation(
                None,
                "route upstream_id is immutable once set",
            ));
        }

        self.validate(tenant_id, &route)?;

        let mut stored = route;
        stored.created_at = existing.created_at;
        stored.updated_at = Some(SystemTime::now());

        self.tables.routes.insert(key, stored.clone());
        Ok(Some(stored))
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool> {
        let _guard = self.lock.lock();
        let key = (tenant_id, id);
        let Some(row) = self.tables.routes.remove(&key) else {
            return Ok(false);
        };

        // Maintain the `(tenant_id, upstream_id)` FK index.
        let idx_key = (tenant_id, row.1.upstream_id);
        if let Some(mut ids) = self.tables.route_upstream.get_mut(&idx_key) {
            ids.retain(|rid| *rid != id);
        }
        Ok(true)
    }
}
