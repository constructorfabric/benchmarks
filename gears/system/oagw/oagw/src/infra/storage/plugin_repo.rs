//! In-memory plugin table with reference tracking.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::{DomainError, ReferencedBy};
use crate::domain::repo::{PluginRecord, PluginRepository, RouteRepository, UpstreamRepository};

/// In-memory plugin table.
///
/// Plugins are immutable (no PUT). A delete is only legal once no upstream or
/// route binds the plugin any more, which is why the table can see the
/// upstream and route tables.
#[derive(Clone)]
pub struct InMemoryPluginRepo {
    rows: Arc<DashMap<String, PluginRecord>>,
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
}

impl std::fmt::Debug for InMemoryPluginRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryPluginRepo")
            .field("plugins", &self.rows.len())
            .finish()
    }
}

impl InMemoryPluginRepo {
    /// Builds a table that scans the given upstream and route tables when
    /// computing `referenced_by`.
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
    ) -> Self {
        Self { rows: Arc::new(DashMap::new()), upstreams, routes }
    }

    fn key(id: &str) -> String {
        id.to_ascii_lowercase()
    }
}

#[async_trait]
impl PluginRepository for InMemoryPluginRepo {
    async fn insert(&self, record: PluginRecord) -> Result<(), DomainError> {
        let key = Self::key(&record.id.to_string());
        if self.rows.contains_key(&key) {
            return Err(DomainError::Conflict {
                detail: format!("plugin `{}` already exists", record.id),
            });
        }
        if self
            .rows
            .iter()
            .any(|r| r.tenant_id == record.tenant_id && r.name == record.name)
        {
            return Err(DomainError::Conflict {
                detail: format!("plugin named `{}` already exists", record.name),
            });
        }
        self.rows.insert(key, record);
        Ok(())
    }

    async fn get(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<Option<PluginRecord>, DomainError> {
        match self.rows.get(&Self::key(id)) {
            Some(row) if row.tenant_id == tenant_id => Ok(Some(row.clone())),
            _ => Ok(None),
        }
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError> {
        Ok(self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect())
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError> {
        match self.rows.get(&Self::key(id)) {
            Some(row) if row.tenant_id == tenant_id => {
                drop(row);
                Ok(self.rows.remove(&Self::key(id)).is_some())
            }
            _ => Ok(false),
        }
    }

    async fn referenced_by(
        &self,
        scope: &[Uuid],
        plugin_id: &str,
    ) -> Result<ReferencedBy, DomainError> {
        let mut out = ReferencedBy::default();
        for tenant in scope {
            for upstream in self.upstreams.list(*tenant).await? {
                for reference in upstream.plugin_refs() {
                    if reference.as_ref_str() == plugin_id
                        || reference.as_ref_str() == uuid_of(plugin_id)
                    {
                        out.upstreams.push(upstream.id.clone().unwrap_or_default());
                    }
                }
            }
        }
        for record in self.routes.list_in_chain(scope).await? {
            for binding in crate::domain::repo::route_bindings(&record.route) {
                let r = binding.reference.as_ref_str();
                if r == plugin_id || r == uuid_of(plugin_id) {
                    out.routes.push(record.route.id.clone().unwrap_or_default());
                }
            }
        }
        Ok(out)
    }
}

/// Renders an id as a bare UUID when it is one.
fn uuid_of(id: &str) -> String {
    match crate::domain::gts_helpers::instance_part(id) {
        Some(part) if Uuid::parse_str(part).is_ok() => part.to_ascii_lowercase(),
        _ => id.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_of_extracts_the_instance_part() {
        let u = uuid::Uuid::from_u128(42);
        assert_eq!(uuid_of(&u.to_string()), u.to_string());
        assert_eq!(
            uuid_of(&format!("gts.cf.core.oagw.auth_plugin.v1~{u}")),
            u.to_string()
        );
        assert_eq!(uuid_of("cf.core.oagw.apikey.v1"), "cf.core.oagw.apikey.v1");
    }
}
