//! In-memory repositories.
//!
//! Upstreams are keyed by `(tenant_id, alias)` and indexed by id; routes and
//! plugins by `(tenant_id, id)`.

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{
    ControlPlaneError, ControlPlaneResult, PluginRepository, RouteRepository, UpstreamRepository,
};

/// In-memory upstream store.
#[derive(Default)]
pub struct MemoryUpstreamRepository {
    rows: DashMap<(Uuid, String), Upstream>,
    by_id: DashMap<String, (Uuid, String)>,
}

impl UpstreamRepository for MemoryUpstreamRepository {
    fn insert(&self, tenant_id: Uuid, upstream: Upstream) -> ControlPlaneResult<()> {
        let Some(id) = upstream.id.clone() else {
            return Err(ControlPlaneError::NotFound);
        };
        let Some(alias_value) = upstream.alias.clone() else {
            return Err(ControlPlaneError::Validation("alias is required".to_owned()));
        };
        let key = (tenant_id, alias_value.clone());
        if self.rows.contains_key(&key) {
            return Err(ControlPlaneError::Conflict(format!(
                "upstream alias '{alias_value}' already exists in this tenant"
            )));
        }
        self.rows.insert(key, upstream);
        self.by_id.insert(id, (tenant_id, alias_value));
        Ok(())
    }

    fn update(&self, tenant_id: Uuid, upstream: Upstream) -> ControlPlaneResult<()> {
        let Some(id) = upstream.id.clone() else {
            return Err(ControlPlaneError::NotFound);
        };
        let Some(old_key) = self.by_id.get(&id).map(|e| e.value().clone()) else {
            return Err(ControlPlaneError::NotFound);
        };
        if old_key.0 != tenant_id {
            return Err(ControlPlaneError::NotFound);
        }
        let Some(alias_value) = upstream.alias.clone() else {
            return Err(ControlPlaneError::NotFound);
        };
        let new_key = (tenant_id, alias_value.clone());
        if new_key != old_key && self.rows.contains_key(&new_key) {
            return Err(ControlPlaneError::Conflict(format!(
                "upstream alias '{alias_value}' already exists in this tenant"
            )));
        }
        if old_key != new_key {
            self.rows.remove(&old_key);
            self.by_id.insert(id, new_key.clone());
        }
        self.rows.insert(new_key, upstream);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: &str) -> Option<Upstream> {
        let (owner, alias_value) = self.by_id.get(id)?.value().clone();
        if owner != tenant_id {
            return None;
        }
        self.rows
            .get(&(owner, alias_value))
            .map(|r| r.value().clone())
    }

    fn delete(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        let Some((owner, alias_value)) = self.by_id.get(id).map(|e| e.value().clone()) else {
            return Err(ControlPlaneError::NotFound);
        };
        if owner != tenant_id {
            return Err(ControlPlaneError::NotFound);
        }
        self.rows.remove(&(owner, alias_value));
        self.by_id.remove(id);
        Ok(())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.rows
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect()
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.rows
            .get(&(tenant_id, alias.to_ascii_lowercase()))
            .map(|r| r.value().clone())
    }

    fn list_all(&self) -> Vec<Upstream> {
        self.rows.iter().map(|r| r.value().clone()).collect()
    }
}

/// In-memory route store.
#[derive(Default)]
pub struct MemoryRouteRepository {
    rows: DashMap<(Uuid, String), Route>,
}

impl RouteRepository for MemoryRouteRepository {
    fn insert(&self, tenant_id: Uuid, route: Route) -> ControlPlaneResult<()> {
        let Some(id) = route.id.clone() else {
            return Err(ControlPlaneError::NotFound);
        };
        self.rows.insert((tenant_id, id), route);
        Ok(())
    }

    fn update(&self, tenant_id: Uuid, route: Route) -> ControlPlaneResult<()> {
        let Some(id) = route.id.clone() else {
            return Err(ControlPlaneError::NotFound);
        };
        let key = (tenant_id, id);
        if !self.rows.contains_key(&key) {
            return Err(ControlPlaneError::NotFound);
        }
        self.rows.insert(key, route);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: &str) -> Option<Route> {
        self.rows
            .get(&(tenant_id, id.to_owned()))
            .map(|r| r.value().clone())
    }

    fn delete(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        self.rows
            .remove(&(tenant_id, id.to_owned()))
            .map(|_| ())
            .ok_or(ControlPlaneError::NotFound)
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        self.rows
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect()
    }

    fn routes_for_upstream(&self, upstream_id: &str) -> Vec<(Uuid, Route)> {
        self.rows
            .iter()
            .filter(|entry| entry.value().upstream_id == upstream_id)
            .map(|entry| (entry.key().0, entry.value().clone()))
            .collect()
    }

    fn list_all(&self) -> Vec<Route> {
        self.rows.iter().map(|r| r.value().clone()).collect()
    }
}

/// In-memory plugin store.
#[derive(Default)]
pub struct MemoryPluginRepository {
    rows: DashMap<(Uuid, String), Plugin>,
}

impl PluginRepository for MemoryPluginRepository {
    fn insert(&self, plugin: Plugin) -> ControlPlaneResult<()> {
        self.rows
            .insert((plugin.tenant_id, plugin.id.clone()), plugin);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: &str) -> Option<Plugin> {
        self.rows
            .get(&(tenant_id, id.to_owned()))
            .map(|r| r.value().clone())
    }

    fn delete(&self, tenant_id: Uuid, id: &str) -> ControlPlaneResult<()> {
        self.rows
            .remove(&(tenant_id, id.to_owned()))
            .map(|_| ())
            .ok_or(ControlPlaneError::NotFound)
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.rows
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect()
    }
}
