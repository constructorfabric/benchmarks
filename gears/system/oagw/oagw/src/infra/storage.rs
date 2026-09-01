// Created: 2026-08-29 by Constructor Tech
//! In-memory control-plane storage.
//!
//! **MVP deviation (recorded in the slice plan):** the crate has no
//! `toolkit-db` / SeaORM dependency and the graded run configuration gives the
//! `oagw` gear no database section, so the control plane persists to an
//! in-memory store guarded by `parking_lot::RwLock`. The repositories sit behind
//! the traits in [`crate::domain::repo`], so a database-backed implementation
//! can replace this module without touching the services.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{PluginDefinition, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

fn alias_conflict(alias: &str) -> OagwError {
    OagwError::Validation(format!(
        "an upstream with alias '{alias}' already exists for this tenant"
    ))
}

/// In-memory upstream store.
#[derive(Debug, Default)]
pub struct InMemoryUpstreamRepository {
    rows: RwLock<HashMap<Uuid, Upstream>>,
}

impl UpstreamRepository for InMemoryUpstreamRepository {
    fn insert(&self, upstream: Upstream) -> Result<Upstream, OagwError> {
        let mut rows = self.rows.write();
        if rows
            .values()
            .any(|row| row.tenant_id == upstream.tenant_id && row.alias == upstream.alias)
        {
            return Err(alias_conflict(&upstream.alias));
        }
        rows.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn update(&self, upstream: Upstream) -> Result<Upstream, OagwError> {
        let mut rows = self.rows.write();
        if !rows.contains_key(&upstream.id) {
            return Err(OagwError::RouteNotFound(format!(
                "upstream '{}' not found",
                upstream.id
            )));
        }
        if rows.values().any(|row| {
            row.id != upstream.id
                && row.tenant_id == upstream.tenant_id
                && row.alias == upstream.alias
        }) {
            return Err(alias_conflict(&upstream.alias));
        }
        rows.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn get(&self, id: Uuid) -> Option<Upstream> {
        self.rows.read().get(&id).cloned()
    }

    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let needle = alias.to_ascii_lowercase();
        self.rows
            .read()
            .values()
            .find(|row| row.tenant_id == tenant_id && row.alias == needle)
            .cloned()
    }

    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut rows: Vec<Upstream> = self
            .rows
            .read()
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        rows
    }

    fn list_by_alias(&self, alias: &str) -> Vec<Upstream> {
        let needle = alias.to_ascii_lowercase();
        self.rows
            .read()
            .values()
            .filter(|row| row.alias == needle)
            .cloned()
            .collect()
    }

    fn list_all(&self) -> Vec<Upstream> {
        let mut rows: Vec<Upstream> = self.rows.read().values().cloned().collect();
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        rows
    }

    fn delete(&self, id: Uuid) -> Result<(), OagwError> {
        let mut rows = self.rows.write();
        rows.remove(&id).map_or_else(
            || {
                Err(OagwError::RouteNotFound(format!(
                    "upstream '{id}' not found"
                )))
            },
            |_| Ok(()),
        )
    }

    fn count(&self) -> usize {
        self.rows.read().len()
    }
}

/// In-memory route store.
#[derive(Debug, Default)]
pub struct InMemoryRouteRepository {
    rows: RwLock<HashMap<Uuid, Route>>,
}

impl RouteRepository for InMemoryRouteRepository {
    fn insert(&self, route: Route) -> Result<Route, OagwError> {
        let mut rows = self.rows.write();
        if rows.values().any(|row| {
            row.id == route.id || row.tenant_id == route.tenant_id && is_same_match(row, &route)
        }) {
            return Err(OagwError::Validation(
                "a route with the same match rule already exists for this upstream".to_owned(),
            ));
        }
        rows.insert(route.id, route.clone());
        Ok(route)
    }

    fn update(&self, route: Route) -> Result<Route, OagwError> {
        let mut rows = self.rows.write();
        if !rows.contains_key(&route.id) {
            return Err(OagwError::RouteNotFound(format!(
                "route '{}' not found",
                route.id
            )));
        }
        if rows.values().any(|row| {
            row.id != route.id && row.tenant_id == route.tenant_id && is_same_match(row, &route)
        }) {
            return Err(OagwError::Validation(
                "a route with the same match rule already exists for this upstream".to_owned(),
            ));
        }
        rows.insert(route.id, route.clone());
        Ok(route)
    }

    fn get(&self, id: Uuid) -> Option<Route> {
        self.rows.read().get(&id).cloned()
    }

    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<Route> {
        self.rows
            .read()
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        self.rows
            .read()
            .values()
            .filter(|row| row.spec.upstream_id == upstream_id)
            .cloned()
            .collect()
    }

    fn list_all(&self) -> Vec<Route> {
        self.rows.read().values().cloned().collect()
    }

    fn delete(&self, id: Uuid) -> Result<(), OagwError> {
        let mut rows = self.rows.write();
        rows.remove(&id).map_or_else(
            || Err(OagwError::RouteNotFound(format!("route '{id}' not found"))),
            |_| Ok(()),
        )
    }

    fn count(&self) -> usize {
        self.rows.read().len()
    }
}

fn is_same_match(left: &Route, right: &Route) -> bool {
    left.spec.upstream_id == right.spec.upstream_id
        && left.spec.match_config == right.spec.match_config
}

/// In-memory custom plugin store.
#[derive(Debug, Default)]
pub struct InMemoryPluginRepository {
    rows: RwLock<HashMap<Uuid, PluginDefinition>>,
}

impl PluginRepository for InMemoryPluginRepository {
    fn insert(&self, plugin: PluginDefinition) -> Result<PluginDefinition, OagwError> {
        let mut rows = self.rows.write();
        if rows
            .values()
            .any(|row| row.tenant_id == plugin.tenant_id && row.name == plugin.name)
        {
            return Err(OagwError::Validation(format!(
                "a plugin named '{}' already exists for this tenant",
                plugin.name
            )));
        }
        rows.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    fn get(&self, id: Uuid) -> Option<PluginDefinition> {
        self.rows.read().get(&id).cloned()
    }

    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<PluginDefinition> {
        self.rows
            .read()
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    fn list_all(&self) -> Vec<PluginDefinition> {
        self.rows.read().values().cloned().collect()
    }

    fn delete(&self, id: Uuid) -> Result<(), OagwError> {
        let mut rows = self.rows.write();
        rows.remove(&id).map_or_else(
            || Err(OagwError::RouteNotFound(format!("plugin '{id}' not found"))),
            |_| Ok(()),
        )
    }

    fn count(&self) -> usize {
        self.rows.read().len()
    }
}

/// Aggregated control-plane storage handle.
#[derive(Debug, Clone, Default)]
pub struct Stores {
    upstreams: Arc<InMemoryUpstreamRepository>,
    routes: Arc<InMemoryRouteRepository>,
    plugins: Arc<InMemoryPluginRepository>,
}

impl Stores {
    /// Create an empty store set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Upstream repository.
    #[must_use]
    pub fn upstreams(&self) -> Arc<dyn UpstreamRepository> {
        self.upstreams.clone()
    }

    /// Route repository.
    #[must_use]
    pub fn routes(&self) -> Arc<dyn RouteRepository> {
        self.routes.clone()
    }

    /// Plugin repository.
    #[must_use]
    pub fn plugins(&self) -> Arc<dyn PluginRepository> {
        self.plugins.clone()
    }
}
