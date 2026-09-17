//! Control-plane domain errors.
//!
//! The domain layer speaks its own vocabulary: no HTTP status codes, no
//! transport types. `crate::api::rest::error` maps these onto the RFC 9457
//! problem catalogue from `DESIGN.md` §3.3.
use thiserror::Error;
use toolkit_macros::domain_model;
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Upstream and route identifiers that reference a plugin, in the anonymous
/// GTS identifier form the API speaks (`gts.cf.core.oagw.{type}.v1~{uuid}`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginReferences {
    /// Upstreams whose plugin chain (or auth plugin) references the plugin.
    pub upstreams: Vec<String>,
    /// Routes whose plugin chain references the plugin.
    pub routes: Vec<String>,
}

impl PluginReferences {
    /// Total number of referencing resources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.upstreams.len() + self.routes.len()
    }

    /// `true` when no upstream and no route references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }

    /// Render the "referenced by 3 upstream(s) and 2 route(s)" phrasing used
    /// by the `PluginInUse` problem `detail` (see `ADR/0001`).
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} upstream(s) and {} route(s)",
            self.upstreams.len(),
            self.routes.len()
        )
    }
}

#[domain_model]
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// A request payload violated a structural rule (field set, enum, range).
    #[error("invalid request: {detail}")]
    Validation { detail: String },
    /// A single field violated its documented shape. `reason` is the stable
    /// machine-readable code surfaced on the wire.
    #[error("field '{field}' violation ({reason}): {detail}")]
    FieldViolation {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },
    /// The addressed resource does not exist for the calling tenant.
    #[error("{detail}")]
    NotFound {
        /// Human-readable, tenant-scoped explanation of what was not found.
        detail: String,
    },
    /// A uniqueness constraint was violated (alias per tenant, plugin name).
    #[error("conflict: {detail}")]
    Conflict { detail: String },
    /// The derived or provided alias collides with an existing upstream of
    /// the same tenant. `alias` is the normalized colliding value.
    #[error("alias '{alias}' is already used by another upstream of this tenant")]
    AliasConflict { alias: String, tenant_id: Uuid },
    /// Two routes of the same upstream claim the same match key.
    #[error("route match conflict: {detail}")]
    RouteMatchConflict { detail: String },
    /// A plugin is still referenced by an upstream or a route.
    #[error("plugin in use: referenced by {references:?}")]
    PluginInUse {
        plugin_id: String,
        references: PluginReferences,
    },
    /// A plugin reference names no known built-in plugin and no plugin row
    /// of the calling tenant, or names a catalog-only identifier.
    #[error("unknown plugin reference: {detail}")]
    UnknownPluginRef { detail: String },
    /// The alias does not follow the endpoint-type rules (`DESIGN.md`
    /// §3.3 "Alias Enforcement Rules" / "Alias Update Behavior").
    #[error("alias rule violation: {detail}")]
    AliasRule { detail: String },
    /// A field that cannot change after creation was sent with a new value.
    #[error("immutable field '{field}' cannot be changed")]
    ImmutableField { field: &'static str },
    #[error("internal error")]
    Internal {
        diagnostic: String,
        #[source]
        cause: Option<BoxError>,
    },
}

impl DomainError {
    /// A plain validation failure carrying only a human-readable detail.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// A single-field validation failure.
    #[must_use]
    pub fn field(field: &'static str, reason: &'static str, detail: impl Into<String>) -> Self {
        Self::FieldViolation {
            field,
            reason,
            detail: detail.into(),
        }
    }

    /// An internal failure with a diagnostic string; the diagnostic never
    /// reaches the wire (`api/rest/error` renders an opaque `detail`).
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
            cause: None,
        }
    }
}
