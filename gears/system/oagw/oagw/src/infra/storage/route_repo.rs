//! In-memory route table.

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::Route;
use crate::domain::error::DomainError;
use crate::domain::repo::{RouteRecord, RouteRepository};

#[derive(Debug, Clone)]
struct Row {
    tenant_id: Uuid,
    seq: u64,
    route: Route,
}

/// In-memory route table enforcing match-rule uniqueness per upstream.
#[derive(Debug, Default)]
pub struct InMemoryRouteRepo {
    rows: DashMap<String, Row>,
    /// `upstream_id` → set of route ids bound to it.
    by_upstream: DashMap<String, Vec<String>>,
    seq: crate::infra::storage::Sequence,
}

impl InMemoryRouteRepo {
    fn key(id: &str) -> String {
        id.to_ascii_lowercase()
    }

    /// The uniqueness signature of a route's match rule.
    fn match_signature(route: &Route) -> String {
        match &route.match_rule.http {
            Some(http) => format!(
                "http|{}|{}",
                http.path,
                http.methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            None => match &route.match_rule.grpc {
                Some(g) => format!("grpc|{}|{}", g.service, g.method),
                None => "none".to_string(),
            },
        }
    }

    /// Whether another route of the same upstream already declares this match.
    async fn signature_taken(&self, upstream_id: &str, route_id: &str, sig: &str) -> bool {
        self.by_upstream
            .get(&upstream_id.to_ascii_lowercase())
            .map(|ids| {
                ids.iter().any(|existing| {
                    existing != route_id
                        && self
                            .rows
                            .get(&Self::key(existing))
                            .map(|r| Self::match_signature(&r.route) == sig)
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }
}

#[async_trait]
impl RouteRepository for InMemoryRouteRepo {
    async fn insert(&self, record: RouteRecord) -> Result<(), DomainError> {
        let id = record
            .route
            .id
            .clone()
            .ok_or_else(|| DomainError::Internal { diagnostic: "route has no id".into() })?;
        let key = Self::key(&id);
        if self.rows.contains_key(&key) {
            return Err(DomainError::Conflict { detail: format!("route `{id}` already exists") });
        }
        let sig = Self::match_signature(&record.route);
        if self
            .signature_taken(&record.route.upstream_id, &id, &sig)
            .await
        {
            return Err(DomainError::Conflict {
                detail: "a route with the same match rule already exists for this upstream"
                    .to_string(),
            });
        }
        self.by_upstream
            .entry(record.route.upstream_id.to_ascii_lowercase())
            .or_default()
            .push(key.clone());
        let seq = self.seq.next();
        self.rows.insert(
            key,
            Row { tenant_id: record.tenant_id, seq, route: record.route },
        );
        Ok(())
    }

    async fn update(&self, record: RouteRecord) -> Result<(), DomainError> {
        let id = record
            .route
            .id
            .clone()
            .ok_or_else(|| DomainError::Internal { diagnostic: "route has no id".into() })?;
        let key = Self::key(&id);
        let (old_upstream, tenant_ok) = match self.rows.get(&key) {
            Some(row) => (row.route.upstream_id.clone(), row.tenant_id == record.tenant_id),
            None => {
                return Err(DomainError::NotFound {
                    detail: format!("route `{id}` does not exist"),
                })
            }
        };
        if !tenant_ok {
            return Err(DomainError::NotFound {
                detail: format!("route `{id}` does not exist"),
            });
        }
        let sig = Self::match_signature(&record.route);
        if self
            .signature_taken(&record.route.upstream_id, &id, &sig)
            .await
        {
            return Err(DomainError::Conflict {
                detail: "a route with the same match rule already exists for this upstream"
                    .to_string(),
            });
        }

        if old_upstream != record.route.upstream_id {
            if let Some(mut ids) = self.by_upstream.get_mut(&old_upstream.to_ascii_lowercase()) {
                ids.retain(|x| x != &key);
            }
            self.by_upstream
                .entry(record.route.upstream_id.to_ascii_lowercase())
                .or_default()
                .push(key.clone());
        }

        let seq = match self.rows.get(&key) {
            Some(row) => row.seq,
            None => self.seq.next(),
        };
        self.rows.insert(key, Row { tenant_id: record.tenant_id, seq, route: record.route });
        Ok(())
    }

    async fn get_by_id(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<Option<Route>, DomainError> {
        match self.rows.get(&Self::key(id)) {
            Some(row) if row.tenant_id == tenant_id => Ok(Some(row.route.clone())),
            _ => Ok(None),
        }
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        let mut rows: Vec<(u64, Route)> = self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| (r.seq, r.route.clone()))
            .collect();
        rows.sort_by_key(|(seq, _)| *seq);
        Ok(rows.into_iter().map(|(_, r)| r).collect())
    }

    async fn list_in_chain(&self, chain: &[Uuid]) -> Result<Vec<RouteRecord>, DomainError> {
        let mut out = Vec::new();
        for tenant in chain {
            let mut rows: Vec<(u64, Route)> = self
                .rows
                .iter()
                .filter(|r| r.tenant_id == *tenant)
                .map(|r| (r.seq, r.route.clone()))
                .collect();
            rows.sort_by_key(|(seq, _)| *seq);
            for (_, route) in rows {
                out.push(RouteRecord { tenant_id: *tenant, route });
            }
        }
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<bool, DomainError> {
        let key = Self::key(id);
        let removed = match self.rows.get(&key) {
            Some(row) if row.tenant_id == tenant_id => {
                let upstream = row.route.upstream_id.clone();
                drop(row);
                if let Some(mut ids) = self.by_upstream.get_mut(&upstream.to_ascii_lowercase()) {
                    ids.retain(|x| x != &key);
                }
                self.rows.remove(&key).is_some()
            }
            _ => false,
        };
        Ok(removed)
    }
}
