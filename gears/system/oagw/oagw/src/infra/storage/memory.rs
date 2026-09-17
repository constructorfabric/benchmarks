//! In-memory implementations of the [`crate::domain::repo`] traits.
//!
//! A single [`MemoryStore`] owns all three tables behind one `parking_lot`
//! `RwLock`, so uniqueness checks in `insert` and read-modify-write sequences
//! are atomic with respect to the whole control plane. There is no eviction
//! and no persistence: rows live for the life of the process, which is what
//! the graded configuration (no `database:` section) asks for.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::error::ErrorKind;
use crate::domain::model::{Plugin, Route, Upstream, normalize_alias};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// All rows owned by the store.
#[derive(Default)]
struct Tables {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, Plugin>,
}

/// Thread-safe in-memory control-plane store.
#[derive(Default)]
pub struct MemoryStore {
    tables: RwLock<Tables>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

fn alias_conflict(alias: &str) -> DomainError {
    DomainError::new(
        ErrorKind::ResourceConflict,
        format!("an upstream with alias `{alias}` already exists in this tenant"),
    )
}

impl UpstreamRepository for MemoryStore {
    fn insert(&self, mut upstream: Upstream) -> Result<(), DomainError> {
        // Aliases are canonical ASCII-lowercase records (PRD: "Aliases are
        // normalized to ASCII lowercase with trailing dots stripped"), so the
        // store normalizes on entry and every lookup compares normalized.
        upstream.alias = normalize_alias(&upstream.alias);
        let mut tables = self.tables.write();
        let taken = tables
            .upstreams
            .values()
            .any(|u| u.tenant_id == upstream.tenant_id && u.alias == upstream.alias);
        if taken {
            return Err(alias_conflict(&upstream.alias));
        }
        tables.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .tables
            .read()
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned())
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError> {
        let wanted = normalize_alias(alias);
        Ok(self
            .tables
            .read()
            .upstreams
            .values()
            .find(|u| u.tenant_id == tenant_id && u.alias == wanted)
            .cloned())
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        Ok(self
            .tables
            .read()
            .upstreams
            .values()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    fn update(&self, upstream: Upstream) -> Result<(), DomainError> {
        let mut tables = self.tables.write();
        match tables.upstreams.get(&upstream.id) {
            Some(existing) if existing.tenant_id != upstream.tenant_id => Err(DomainError::new(
                ErrorKind::ResourceNotFound,
                "upstream does not exist in this tenant",
            )),
            Some(_) => {
                let taken = tables.upstreams.values().any(|u| {
                    u.id != upstream.id
                        && u.tenant_id == upstream.tenant_id
                        && u.alias == upstream.alias
                });
                if taken {
                    return Err(alias_conflict(&upstream.alias));
                }
                tables.upstreams.insert(upstream.id, upstream);
                Ok(())
            }
            None => Err(DomainError::new(
                ErrorKind::ResourceNotFound,
                "upstream does not exist in this tenant",
            )),
        }
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let mut tables = self.tables.write();
        match tables.upstreams.get(&id) {
            Some(u) if u.tenant_id == tenant_id => Ok(tables.upstreams.remove(&id).is_some()),
            _ => Ok(false),
        }
    }
}

impl RouteRepository for MemoryStore {
    fn insert(&self, route: Route) -> Result<(), DomainError> {
        let mut tables = self.tables.write();
        let taken = tables.routes.values().any(|r| {
            r.tenant_id == route.tenant_id
                && r.upstream_id == route.upstream_id
                && r.spec.r#match.key() == route.spec.r#match.key()
                && r.spec.priority == route.spec.priority
        });
        if taken {
            return Err(DomainError::new(
                ErrorKind::ResourceConflict,
                "a route with the same match rule and priority already exists for this upstream",
            ));
        }
        tables.routes.insert(route.id, route);
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self
            .tables
            .read()
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned())
    }

    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Result<Vec<Route>, DomainError> {
        Ok(self
            .tables
            .read()
            .routes
            .values()
            .filter(|r| r.tenant_id == tenant_id && upstream_id.is_none_or(|u| r.upstream_id == u))
            .cloned()
            .collect())
    }

    fn update(&self, route: Route) -> Result<(), DomainError> {
        let mut tables = self.tables.write();
        match tables.routes.get(&route.id) {
            Some(existing) if existing.tenant_id != route.tenant_id => Err(DomainError::new(
                ErrorKind::ResourceNotFound,
                "route does not exist in this tenant",
            )),
            Some(_) => {
                let taken = tables.routes.values().any(|r| {
                    r.id != route.id
                        && r.tenant_id == route.tenant_id
                        && r.upstream_id == route.upstream_id
                        && r.spec.r#match.key() == route.spec.r#match.key()
                        && r.spec.priority == route.spec.priority
                });
                if taken {
                    return Err(DomainError::new(
                        ErrorKind::ResourceConflict,
                        "a route with the same match rule and priority already exists for \
                         this upstream",
                    ));
                }
                tables.routes.insert(route.id, route);
                Ok(())
            }
            None => Err(DomainError::new(
                ErrorKind::ResourceNotFound,
                "route does not exist in this tenant",
            )),
        }
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let mut tables = self.tables.write();
        match tables.routes.get(&id) {
            Some(r) if r.tenant_id == tenant_id => Ok(tables.routes.remove(&id).is_some()),
            _ => Ok(false),
        }
    }

    fn delete_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<u64, DomainError> {
        let mut tables = self.tables.write();
        let doomed: Vec<Uuid> = tables
            .routes
            .iter()
            .filter(|(_, r)| r.tenant_id == tenant_id && r.upstream_id == upstream_id)
            .map(|(id, _)| *id)
            .collect();
        let removed = doomed.len();
        for id in doomed {
            tables.routes.remove(&id);
        }
        Ok(u64::try_from(removed).unwrap_or(0))
    }
}

impl PluginRepository for MemoryStore {
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        self.tables.write().plugins.insert(plugin.id, plugin);
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        Ok(self
            .tables
            .read()
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .cloned())
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        Ok(self
            .tables
            .read()
            .plugins
            .values()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let mut tables = self.tables.write();
        match tables.plugins.get(&id) {
            Some(p) if p.tenant_id == tenant_id => Ok(tables.plugins.remove(&id).is_some()),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;
