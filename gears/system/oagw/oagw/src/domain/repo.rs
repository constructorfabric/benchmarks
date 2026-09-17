//! Repository contracts for OAGW control-plane persistence.
//!
//! Storage is deliberately behind traits so the domain stays agnostic;
//! the in-memory implementation lives in `infra::storage`. Every CRUD
//! operation is tenant-scoped — ancestor resources are invisible to
//! descendants via the management plane (DOCS §1.2), so the traits take
//! the calling `tenant_id` and the repo returns only rows owned by it.
//! Hierarchy-aware reads (alias resolution that walks the tenant chain)
//! are expressed explicitly via the `tenant_chain` parameters.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Plugin, Route, Upstream};

/// Tenant-scoped upstream repository.
#[async_trait]
pub trait UpstreamRepo: Send + Sync {
    /// Insert a new upstream (caller guarantees alias uniqueness).
    ///
    /// # Errors
    /// `DomainError::AlreadyExists` when an upstream with the same id
    /// already exists.
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Fetch an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;

    /// Replace an upstream owned by `tenant_id` in full.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn replace(&self, tenant_id: Uuid, upstream: Upstream) -> Result<(), DomainError>;

    /// Delete an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// All upstreams owned by `tenant_id` (management listing).
    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// Whether an upstream owned by `tenant_id` uses this alias.
    async fn alias_exists(&self, tenant_id: Uuid, alias: &str) -> bool;

    /// Resolve `alias` among the tenants in `tenant_chain` (ordered
    /// descendant → root). The closest tenant wins (shadowing).
    async fn resolve_alias(&self, tenant_chain: &[Uuid], alias: &str) -> Option<Upstream>;

    /// Fetch an upstream by id regardless of tenant (in-use / bind
    /// checks across the hierarchy).
    async fn get_any_tenant(&self, id: Uuid) -> Option<Upstream>;

    /// Every upstream in the store (plugin in-use reporting).
    async fn all_upstreams(&self) -> Vec<Upstream>;
}

/// Tenant-scoped route repository.
#[async_trait]
pub trait RouteRepo: Send + Sync {
    /// Insert a new route (caller guarantees match uniqueness).
    ///
    /// # Errors
    /// `DomainError::AlreadyExists` when a route with the same id exists.
    async fn insert(&self, route: Route) -> Result<(), DomainError>;

    /// Fetch a route owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;

    /// Replace a route owned by `tenant_id` in full.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn replace(&self, tenant_id: Uuid, route: Route) -> Result<(), DomainError>;

    /// Delete a route owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// All routes owned by `tenant_id` (management listing).
    async fn list(&self, tenant_id: Uuid) -> Vec<Route>;

    /// All routes owned by any tenant in `tenant_chain` (ordered
    /// descendant → root, used for proxy matching with descendant
    /// priority).
    async fn list_for_chain(&self, tenant_chain: &[Uuid]) -> Vec<Route>;

    /// Fetch a route by id regardless of tenant (in-use checks).
    async fn get_any_tenant(&self, id: Uuid) -> Option<Route>;

    /// Every route in the store (plugin in-use reporting).
    async fn all_routes(&self) -> Vec<Route>;
}

/// Custom plugin repository (UUID-backed plugin definitions).
#[async_trait]
pub trait PluginRepo: Send + Sync {
    /// Insert a custom plugin definition.
    ///
    /// # Errors
    /// `DomainError::AlreadyExists` when the id (or the tenant-scoped
    /// name) collides.
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Fetch a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;

    /// Delete a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// `DomainError::NotFound` when absent or owned by another tenant.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// All plugins owned by `tenant_id` (management listing).
    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin>;

    /// Fetch a plugin by uuid regardless of tenant (binding resolution).
    async fn get_any_tenant(&self, id: Uuid) -> Option<Plugin>;

    /// Every plugin in the store.
    async fn all_plugins(&self) -> Vec<Plugin>;
}
