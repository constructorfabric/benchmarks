//! Repository traits and shared result/error types.

use crate::domain::model::{Plugin, Route, Upstream};

/// Error returned by control-plane operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneError {
    /// Validation failed.
    Validation(String),
    /// The resource does not exist in this tenant.
    NotFound,
    /// A uniqueness constraint was violated on an upstream alias.
    Conflict(String),
    /// Two enabled routes share a path, method and priority.
    RouteConflict(String),
    /// The referenced resource is still in use.
    InUse {
        /// Where it is referenced, grouped by resource kind.
        referenced_by: serde_json::Value,
        /// Human-readable summary of the same references.
        detail: String,
    },
    /// A referenced plugin has no implementation.
    UnknownPlugin(String),
    /// The referenced upstream does not exist in this tenant.
    UnknownUpstream,
}

impl ControlPlaneError {
    /// True when this is a validation error, which always maps to `400`.
    #[must_use]
    pub fn is_validation(&self) -> bool {
        matches!(
            self,
            Self::Validation(_) | Self::UnknownPlugin(_) | Self::UnknownUpstream
        )
    }
}

impl std::fmt::Display for ControlPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(detail) | Self::Conflict(detail) | Self::RouteConflict(detail) => {
                write!(f, "{detail}")
            }
            Self::InUse { detail, .. } => write!(f, "{detail}"),
            Self::UnknownPlugin(id) => write!(f, "unknown auth plugin {id}"),
            Self::NotFound | Self::UnknownUpstream => write!(f, "not found"),
        }
    }
}

impl std::error::Error for ControlPlaneError {}

/// Result alias for control-plane operations.
pub type ControlPlaneResult<T> = Result<T, ControlPlaneError>;

/// Storage for upstreams.
pub trait UpstreamRepository: Send + Sync {
    /// Inserts an upstream, rejecting a `(tenant_id, alias)` collision.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::Conflict`] when the alias is taken.
    fn insert(&self, tenant_id: uuid::Uuid, upstream: Upstream) -> ControlPlaneResult<()>;

    /// Replaces an upstream by id within the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    fn update(&self, tenant_id: uuid::Uuid, upstream: Upstream) -> ControlPlaneResult<()>;

    /// Reads an upstream by id within the tenant.
    fn get(&self, tenant_id: uuid::Uuid, id: &str) -> Option<Upstream>;

    /// Deletes an upstream by id within the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    fn delete(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists every upstream owned by the tenant.
    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Upstream>;

    /// Lists every upstream across all tenants, for reference checks.
    fn list_all(&self) -> Vec<Upstream>;

    /// Finds an upstream by alias within the tenant.
    fn find_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream>;
}

/// Storage for routes.
pub trait RouteRepository: Send + Sync {
    /// Inserts a route.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] when the id is already present.
    fn insert(&self, tenant_id: uuid::Uuid, route: Route) -> ControlPlaneResult<()>;

    /// Replaces a route by id within the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    fn update(&self, tenant_id: uuid::Uuid, route: Route) -> ControlPlaneResult<()>;

    /// Reads a route by id within the tenant.
    fn get(&self, tenant_id: uuid::Uuid, id: &str) -> Option<Route>;

    /// Deletes a route by id within the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    fn delete(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists every route owned by the tenant.
    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Route>;

    /// Lists every route across all tenants, for reference checks.
    fn list_all(&self) -> Vec<Route>;

    /// Lists every route of an upstream across all tenants.
    fn routes_for_upstream(&self, upstream_id: &str) -> Vec<(uuid::Uuid, Route)>;
}

/// Storage for custom plugins.
pub trait PluginRepository: Send + Sync {
    /// Inserts a plugin.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] when the id is already present.
    fn insert(&self, plugin: Plugin) -> ControlPlaneResult<()>;

    /// Reads a plugin by id within the tenant.
    fn get(&self, tenant_id: uuid::Uuid, id: &str) -> Option<Plugin>;

    /// Deletes a plugin by id within the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    fn delete(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists every plugin owned by the tenant.
    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Plugin>;
}
