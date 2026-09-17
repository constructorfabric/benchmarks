//! DashMap-backed [`UpstreamRepository`] implementation.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::entity::Upstream;
use crate::domain::entity::alias::{validate_alias, validate_endpoint_url};
use crate::domain::error::DomainError;
use crate::domain::repo::{RepoResult, UpstreamRepository};

use super::StoreTables;

/// Configuration influencing the configuration-boundary guards applied by the
/// upstream repository (aliases, endpoint SSRF).
#[derive(Debug, Clone, Copy, Default)]
pub struct UpstreamRepoOptions {
    /// Opt-in to plaintext HTTP upstream endpoints at the configuration
    /// boundary (default `false` keeps the outbound surface HTTPS-only).
    pub allow_http_upstream: bool,
}

/// In-memory `oagw_upstream` implementation.
pub struct InMemoryUpstreamRepo {
    tables: Arc<StoreTables>,
    lock: Arc<Mutex<()>>,
    options: UpstreamRepoOptions,
}

impl InMemoryUpstreamRepo {
    #[must_use]
    pub(crate) fn new(
        tables: Arc<StoreTables>,
        lock: Arc<Mutex<()>>,
        options: UpstreamRepoOptions,
    ) -> Self {
        Self {
            tables,
            lock,
            options,
        }
    }

    /// Configuration-boundary guards: alias pattern + endpoint SSRF.
    fn validate_for_persist(&self, upstream: &Upstream) -> Result<(), DomainError> {
        validate_alias(&upstream.alias).map_err(|e| DomainError::validation(None, e))?;
        for ep in &upstream.server.endpoints {
            validate_endpoint_url(ep, self.options.allow_http_upstream)
                .map_err(|e| DomainError::validation(None, e))?;
        }
        Ok(())
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryUpstreamRepo {
    async fn create(&self, tenant_id: Uuid, upstream: Upstream) -> RepoResult<Upstream> {
        self.validate_for_persist(&upstream)?;

        let mut stored = upstream;
        let now = SystemTime::now();
        stored.created_at = Some(now);
        stored.updated_at = Some(now);

        let _guard = self.lock.lock();
        let alias_key = (tenant_id, stored.alias.clone());
        if self.tables.upstream_alias.contains_key(&alias_key) {
            return Err(DomainError::validation(
                None,
                format!(
                    "upstream alias '{}' already exists within the tenant",
                    stored.alias
                ),
            ));
        }

        let key = (tenant_id, stored.id);
        self.tables.upstream_alias.insert(alias_key, stored.id);
        self.tables.upstreams.insert(key, stored.clone());
        Ok(stored)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.tables
            .upstreams
            .get(&(tenant_id, id))
            .map(|r| r.clone())
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let id = self
            .tables
            .upstream_alias
            .get(&(tenant_id, alias.to_owned()))?;
        self.tables
            .upstreams
            .get(&(tenant_id, *id))
            .map(|r| r.clone())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut rows: Vec<Upstream> = self
            .tables
            .upstreams
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        rows.sort_by_key(|u| u.id);
        rows
    }

    async fn update(&self, tenant_id: Uuid, upstream: Upstream) -> RepoResult<Option<Upstream>> {
        self.validate_for_persist(&upstream)?;

        let _guard = self.lock.lock();
        let key = (tenant_id, upstream.id);
        // Clone out of the DashMap read guard: the `Ref` would otherwise hold
        // the shard read lock while `insert` below needs the write lock,
        // deadlocking on the same thread.
        let Some(existing) = self.tables.upstreams.get(&key).map(|r| r.value().clone()) else {
            return Ok(None);
        };

        // `alias` is immutable once set.
        if existing.alias != upstream.alias {
            return Err(DomainError::validation(
                None,
                format!(
                    "upstream alias is immutable (existing '{}', attempted '{}')",
                    existing.alias, upstream.alias
                ),
            ));
        }

        let mut stored = upstream;
        stored.updated_at = Some(SystemTime::now());
        self.tables.upstreams.insert(key, stored.clone());
        Ok(Some(stored))
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool> {
        let _guard = self.lock.lock();
        let key = (tenant_id, id);
        let Some(row) = self.tables.upstreams.remove(&key) else {
            return Ok(false);
        };

        // Cascade to routes (`oagw_route.upstream_id` FK ON DELETE CASCADE).
        let route_idx_key = (tenant_id, id);
        if let Some((_, route_ids)) = self.tables.route_upstream.remove(&route_idx_key) {
            for route_id in route_ids {
                self.tables.routes.remove(&(tenant_id, route_id));
            }
        }

        self.tables.upstream_alias.remove(&(tenant_id, row.1.alias));
        Ok(true)
    }
}
