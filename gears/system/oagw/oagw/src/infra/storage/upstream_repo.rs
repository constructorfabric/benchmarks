//! In-memory upstream table.

use std::collections::HashMap;

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::Upstream;
use crate::domain::error::DomainError;
use crate::domain::repo::{UpstreamRecord, UpstreamRepository};

#[derive(Debug, Clone)]
struct Row {
    tenant_id: Uuid,
    seq: u64,
    upstream: Upstream,
}

/// In-memory upstream table enforcing `UNIQUE (tenant_id, alias)`.
#[derive(Debug, Default)]
pub struct InMemoryUpstreamRepo {
    rows: DashMap<String, Row>,
    by_alias: DashMap<(Uuid, String), String>,
    seq: crate::infra::storage::Sequence,
}

impl InMemoryUpstreamRepo {
    fn key(id: &str) -> String {
        id.to_ascii_lowercase()
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryUpstreamRepo {
    async fn insert(&self, record: UpstreamRecord) -> Result<(), DomainError> {
        let id = record
            .upstream
            .id
            .clone()
            .ok_or_else(|| DomainError::Internal { diagnostic: "upstream has no id".into() })?;
        let key = Self::key(&id);
        if self.rows.contains_key(&key) {
            return Err(DomainError::Conflict {
                detail: format!("upstream `{id}` already exists"),
            });
        }
        if let Some(alias) = record.upstream.alias.clone() {
            if !alias.is_empty()
                && self.by_alias.contains_key(&(record.tenant_id, alias.clone()))
            {
                return Err(DomainError::Conflict {
                    detail: format!("upstream with alias `{alias}` already exists"),
                });
            }
            self.by_alias.insert((record.tenant_id, alias), key.clone());
        }
        let seq = self.seq.next();
        self.rows.insert(
            key,
            Row { tenant_id: record.tenant_id, seq, upstream: record.upstream },
        );
        Ok(())
    }

    async fn update(&self, record: UpstreamRecord) -> Result<(), DomainError> {
        let id = record
            .upstream
            .id
            .clone()
            .ok_or_else(|| DomainError::Internal { diagnostic: "upstream has no id".into() })?;
        let key = Self::key(&id);
        let mut entry = match self.rows.get_mut(&key) {
            Some(e) => e,
            None => {
                return Err(DomainError::NotFound {
                    detail: format!("upstream `{id}` does not exist"),
                })
            }
        };
        if entry.tenant_id != record.tenant_id {
            return Err(DomainError::NotFound {
                detail: format!("upstream `{id}` does not exist"),
            });
        }

        let old_alias = entry.upstream.alias.clone();
        let new_alias = record.upstream.alias.clone();
        if old_alias != new_alias {
            if let Some(alias) = new_alias.clone() {
                if !alias.is_empty()
                    && self
                        .by_alias
                        .contains_key(&(record.tenant_id, alias.clone()))
                {
                    return Err(DomainError::Conflict {
                        detail: format!("upstream with alias `{alias}` already exists"),
                    });
                }
            }
            if let Some(old) = old_alias {
                self.by_alias.remove(&(record.tenant_id, old));
            }
            if let Some(alias) = new_alias {
                self.by_alias.insert((record.tenant_id, alias), key.clone());
            }
        }
        entry.upstream = record.upstream;
        drop(entry);
        Ok(())
    }

    async fn get_by_id(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        match self.rows.get(&Self::key(id)) {
            Some(row) if row.tenant_id == tenant_id => Ok(Some(row.upstream.clone())),
            _ => Ok(None),
        }
    }

    async fn get_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        let normalised = crate::domain::alias::normalise_alias(alias);
        match self.by_alias.get(&(tenant_id, normalised)) {
            Some(key) => Ok(self.rows.get(&*key).map(|r| r.upstream.clone())),
            None => Ok(None),
        }
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let mut rows: Vec<(u64, Upstream)> = self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| (r.seq, r.upstream.clone()))
            .collect();
        rows.sort_by_key(|(seq, _)| *seq);
        Ok(rows.into_iter().map(|(_, u)| u).collect())
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError> {
        let key = Self::key(id);
        // The shard guard from `rows.get` must be gone before any write to the
        // same map: holding it across `remove` self-deadlocks the shard.
        let owned = match self.rows.get(&key) {
            Some(row) if row.tenant_id == tenant_id => {
                Some((key.clone(), row.upstream.alias.clone()))
            }
            _ => None,
        };
        let Some((key, alias)) = owned else {
            return Ok(false);
        };
        if let Some(alias) = alias {
            self.by_alias.remove(&(tenant_id, alias));
        }
        Ok(self.rows.remove(&key).is_some())
    }

    async fn find_in_chain(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Result<Option<(Uuid, Upstream)>, DomainError> {
        let normalised = crate::domain::alias::normalise_alias(alias);
        for tenant in chain {
            if let Some(key) = self.by_alias.get(&(*tenant, normalised.clone())) {
                if let Some(row) = self.rows.get(&*key) {
                    return Ok(Some((row.tenant_id, row.upstream.clone())));
                }
            }
        }
        Ok(None)
    }
}

/// Internal helper used by the plugin reference scan.
impl InMemoryUpstreamRepo {
    /// Lists every upstream in a tenant chain, descendant-first.
    pub async fn list_in_chain(
        &self,
        chain: &[Uuid],
    ) -> Vec<(Uuid, Upstream)> {
        let mut out = Vec::new();
        for tenant in chain {
            for item in self.rows.iter() {
                if item.tenant_id == *tenant {
                    out.push((*tenant, item.upstream.clone()));
                }
            }
        }
        out
    }

    /// Whether the alias table carries an entry (diagnostics only).
    pub fn alias_exists(&self, tenant_id: Uuid, alias: &str) -> bool {
        self.by_alias
            .contains_key(&(tenant_id, crate::domain::alias::normalise_alias(alias)))
    }

    /// Snapshot of the alias index, for tests.
    pub fn alias_index(&self) -> HashMap<(Uuid, String), String> {
        self.by_alias
            .iter()
            .map(|kv| ((kv.key().0.clone(), kv.key().1.clone()), kv.value().clone()))
            .collect()
    }
}
