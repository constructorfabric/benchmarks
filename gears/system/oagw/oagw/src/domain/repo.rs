//! Repository traits (DESIGN §3.2 `domain/repo.rs`).
//!
//! The control plane persists configuration through these traits. The
//! production deployment uses the relational schema in DESIGN §3.6; this crate
//! ships an in-memory implementation (`infra::storage`) because the crate
//! manifest carries no database dependency.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::model::{Plugin, Route, Upstream};

/// Persistence for upstream entities.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream.
    ///
    /// # Errors
    ///
    /// Returns `AliasConflict` when `(tenant_id, alias)` is already bound.
    async fn insert(&self, upstream: Upstream) -> DomainResult<()>;

    /// Replaces an existing upstream.
    ///
    /// # Errors
    /// Returns an error when the upstream does not exist or the alias is taken
    /// by another upstream of the same tenant.
    async fn replace(&self, upstream: Upstream) -> DomainResult<()>;

    /// Fetches an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Upstream>>;

    /// Fetches an upstream by alias within `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> DomainResult<Option<Upstream>>;

    /// Lists every upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn list_by_tenant(&self, tenant_id: Uuid) -> DomainResult<Vec<Upstream>>;

    /// Lists every upstream across tenants (data-plane resolution input).
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn list_all(&self) -> DomainResult<Vec<Upstream>>;

    /// Deletes an upstream owned by `tenant_id`; returns `false` when absent.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;
}

/// Persistence for route entities.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Inserts a new route.
    ///
    /// # Errors
    /// Returns an error when the route already exists.
    async fn insert(&self, route: Route) -> DomainResult<()>;

    /// Replaces an existing route.
    ///
    /// # Errors
    /// Returns an error when the route does not exist.
    async fn replace(&self, route: Route) -> DomainResult<()>;

    /// Fetches a route owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Route>>;

    /// Lists routes owned by `tenant_id`, optionally filtered by upstream.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn list_by_tenant(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> DomainResult<Vec<Route>>;

    /// Lists every route across tenants (data-plane resolution input).
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn list_all(&self) -> DomainResult<Vec<Route>>;

    /// Deletes a route owned by `tenant_id`; returns `false` when absent.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;
}

/// Persistence for custom (Starlark) plugin entities.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Inserts a new plugin.
    ///
    /// # Errors
    /// Returns an error when `(tenant_id, name)` already exists.
    async fn insert(&self, plugin: Plugin) -> DomainResult<()>;

    /// Fetches a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Plugin>>;

    /// Fetches a plugin by tenant and name.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> DomainResult<Option<Plugin>>;

    /// Lists plugins owned by `tenant_id`, optionally filtered by type.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    async fn list_by_tenant(
        &self,
        tenant_id: Uuid,
        plugin_type: Option<&str>,
    ) -> DomainResult<Vec<Plugin>>;

    /// Marks a plugin as used.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    async fn touch(&self, tenant_id: Uuid, id: Uuid, now_millis: u64) -> DomainResult<()>;

    /// Marks an unlinked plugin as garbage-collectable.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    async fn mark_gc_eligible(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        eligible_at_millis: u64,
    ) -> DomainResult<()>;

    /// Deletes a plugin owned by `tenant_id`; returns `false` when absent.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;
}
