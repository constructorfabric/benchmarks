//! Upstream CRUD (control plane).
//!
//! Enforces the DESIGN.md §3.1 alias rules and the §3.4 CRUD semantics:
//! server-generated ids, `(tenant_id, alias)` uniqueness, full-replacement PUT
//! semantics and strict tenant scoping.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias::{
    enforce_alias_update_with, normalize_alias, resolve_alias_for_spec,
};
use crate::domain::error::DomainError;
use crate::domain::models::{Route, Upstream, UpstreamSpec, validate_upstream_spec};
use crate::domain::repo::{Repositories, RouteRepository, UpstreamRepository};
use crate::domain::services::ListQuery;

/// Upstream management operations.
#[derive(Clone)]
pub struct UpstreamService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    /// L1 data-plane cache, invalidated on every write (ADR 0005).
    cache: Option<Arc<crate::infra::cache::ConfigCache>>,
}

impl std::fmt::Debug for UpstreamService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamService").finish_non_exhaustive()
    }
}

impl UpstreamService {
    /// Builds a service over the repository bundle.
    #[must_use]
    pub fn new(repos: &Repositories) -> Self {
        Self {
            upstreams: repos.upstreams.clone(),
            routes: repos.routes.clone(),
            cache: None,
        }
    }

    /// Shares the data plane's L1 cache with this service, so a write
    /// invalidates the proxied requests' copy in the same tick.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::infra::cache::ConfigCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Bumps the L1 cache generation after a successful write.
    fn invalidate(&self) {
        if let Some(cache) = self.cache.as_ref() {
            cache.invalidate();
        }
    }

    /// Creates an upstream from a validated spec.
    ///
    /// # Errors
    ///
    /// * [`DomainError::ImmutableField`] (409) when `id`/`tenant_id` are supplied.
    /// * [`DomainError::Validation`] (400) for an invalid body or alias.
    /// * [`DomainError::AliasConflict`] (409) when the alias is already taken.
    pub fn create(&self, tenant_id: Uuid, spec: UpstreamSpec) -> Result<Upstream, DomainError> {
        check_immutable_ids(tenant_id, spec.id, spec.tenant_id)?;
        validate_upstream_spec(&spec)?;
        let (_, alias) =
            resolve_alias_for_spec(spec.server.endpoints.as_slice(), spec.alias.as_deref())?;
        ensure_alias_free(&*self.upstreams, tenant_id, &alias, None)?;

        let upstream = Upstream::from_spec(spec, Uuid::new_v4(), tenant_id, alias);
        let inserted = self.upstreams.insert(upstream)?;
        self.invalidate();
        Ok(inserted)
    }

    /// Replaces an upstream (full replacement; omitted optionals are cleared).
    ///
    /// # Errors
    ///
    /// * [`DomainError::NotFound`] (404) for an unknown id.
    /// * [`DomainError::ImmutableField`] (409) for `id`/`tenant_id` overrides.
    /// * [`DomainError::Validation`] (400) for an alias-changing endpoint edit.
    /// * [`DomainError::AliasConflict`] (409) on an alias collision.
    pub fn replace(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        check_immutable_ids(tenant_id, spec.id, spec.tenant_id)?;
        let existing = self
            .upstreams
            .find(tenant_id, id)?
            .ok_or_else(|| DomainError::NotFound(upstream_not_found(id)))?;
        validate_upstream_spec(&spec)?;

        let alias = enforce_alias_update_with(
            &existing.alias,
            existing.server.endpoints.as_slice(),
            spec.server.endpoints.as_slice(),
            spec.alias.as_deref(),
        )?;
        ensure_alias_free(&*self.upstreams, tenant_id, &alias, Some(existing.id))?;

        let upstream = Upstream::from_spec(spec, existing.id, existing.tenant_id, alias);
        let replaced = self.upstreams.replace(upstream)?;
        self.invalidate();
        Ok(replaced)
    }

    /// Loads one upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the id is unknown to this tenant.
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .find(tenant_id, id)?
            .ok_or_else(|| DomainError::NotFound(upstream_not_found(id)))
    }

    /// Lists the upstreams of one tenant, applying the list-query subset.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    pub fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Vec<Upstream>, DomainError> {
        let mut items = self.upstreams.list(tenant_id)?;
        if let Some(alias) = query.filter_value("alias") {
            items.retain(|u| normalize_alias(&u.alias) == normalize_alias(alias));
        }
        if let Some(tag) = query.filter_value("tag") {
            items.retain(|u| u.tags.iter().any(|t| t == tag));
        }
        if let Some((field, descending)) = &query.order_by {
            match field.as_str() {
                "alias" => sort_by(&mut items, *descending, |u| u.alias.clone()),
                "id" => sort_by(&mut items, *descending, |u| u.id.to_string()),
                _ => {}
            }
        }
        Ok(page(&mut items, query))
    }

    /// Deletes an upstream and cascades to its routes.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the id is unknown to this tenant.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let existing = self
            .upstreams
            .find(tenant_id, id)?
            .ok_or_else(|| DomainError::NotFound(upstream_not_found(id)))?;
        self.routes.delete_by_upstream(existing.id)?;
        self.upstreams.delete(tenant_id, id)?;
        self.invalidate();
        Ok(())
    }

    /// Routes of one upstream, in creation order.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is unknown to this tenant.
    pub fn routes_of(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.get(tenant_id, upstream_id)?;
        self.routes.list_by_upstream(upstream_id)
    }
}

/// Sorts `items` by `key`, ascending or descending, in place.
fn sort_by<T, F>(items: &mut [T], descending: bool, mut key: F)
where
    F: FnMut(&T) -> String,
{
    if descending {
        items.sort_by_key(|a| std::cmp::Reverse(key(a)));
    } else {
        items.sort_by_key(|a| key(a));
    }
}

/// Applies `$skip`/`$top` (default 50, max 100) and consumes `items`.
fn page<T>(items: &mut Vec<T>, query: &ListQuery) -> Vec<T> {
    let top = query.top.unwrap_or(50).min(100);
    let skip = query.skip.unwrap_or(0);
    if skip >= items.len() {
        items.clear();
        return Vec::new();
    }
    items.drain(..skip);
    items.truncate(top);
    std::mem::take(items)
}

/// Rejects caller-supplied `id`/`tenant_id` values as immutable.
///
/// # Errors
///
/// [`DomainError::ImmutableField`] with `resource`/`field`.
fn check_immutable_ids(
    tenant_id: Uuid,
    id: Option<Uuid>,
    supplied_tenant: Option<Uuid>,
) -> Result<(), DomainError> {
    if id.is_some() {
        return Err(DomainError::ImmutableField {
            resource: "upstream",
            field: "id",
        });
    }
    if supplied_tenant.is_some() && supplied_tenant != Some(tenant_id) {
        return Err(DomainError::ImmutableField {
            resource: "upstream",
            field: "tenant_id",
        });
    }
    Ok(())
}

/// Fails when `(tenant_id, alias)` is already taken by another upstream.
///
/// # Errors
///
/// [`DomainError::AliasConflict`] with the conflicting id.
fn ensure_alias_free(
    repo: &dyn UpstreamRepository,
    tenant_id: Uuid,
    alias: &str,
    allowed: Option<Uuid>,
) -> Result<(), DomainError> {
    if let Some(existing) = repo.alias_owner(tenant_id, alias)?
        && Some(existing) != allowed
    {
        return Err(DomainError::AliasConflict {
            alias: alias.to_owned(),
            existing_id: existing,
        });
    }
    Ok(())
}

/// 404 detail for an unknown upstream id.
fn upstream_not_found(id: Uuid) -> String {
    format!("no upstream with id {id}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn page_is_deterministic() {
        let mut items = vec![1, 2, 3, 4];
        let query = ListQuery {
            filters: Vec::new(),
            order_by: None,
            top: Some(2),
            skip: Some(1),
        };
        assert_eq!(page(&mut items, &query), vec![2, 3]);
        assert!(items.is_empty());
    }

    #[test]
    fn page_clamps_top_at_one_hundred() {
        let mut items = vec![0; 150];
        let query = ListQuery {
            filters: Vec::new(),
            order_by: None,
            top: Some(500),
            skip: None,
        };
        assert_eq!(page(&mut items, &query).len(), 100);
    }

    #[test]
    fn page_beyond_the_end_is_empty() {
        let mut items = vec![1, 2];
        let query = ListQuery {
            filters: Vec::new(),
            order_by: None,
            top: None,
            skip: Some(9),
        };
        assert!(page(&mut items, &query).is_empty());
    }

    #[test]
    fn immutable_ids_are_reported() {
        let err = check_immutable_ids(Uuid::nil(), Some(Uuid::new_v4()), None).unwrap_err();
        assert_eq!(err.http_status(), 409);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        let err = check_immutable_ids(Uuid::nil(), None, Some(Uuid::new_v4())).unwrap_err();
        assert_eq!(err.http_status(), 409);
        assert!(check_immutable_ids(Uuid::nil(), None, Some(Uuid::nil())).is_ok());
    }
}
