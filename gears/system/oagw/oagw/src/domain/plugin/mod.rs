//! Plugin-execution traits (ADR 0002 as refined by ADR 0008) and the four
//! execution payloads.
//!
//! ADR 0002 prints `authenticate(&self, ctx: &mut RequestContext)`; ADR 0008 —
//! the later, more specific ADR that `features/plugin-system.md` builds on —
//! types the auth payload as [`AuthContext`] and returns `Result<(),
//! PluginError>`. ADR 0008 wins: the payload-type set is `AuthContext` for
//! auth, `RequestContext` / `ResponseContext` for guard and transform, and
//! `ErrorContext` for the error phase.
//!
//! Entry 2.1 declares the traits and payloads only; the registries and
//! built-in implementations live in `infra/plugin/` and are filled by entry
//! 2.6. `basic` / `bearer` are catalog-only identifiers with no backing
//! implementation here (graded deviation 10).

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;

pub mod composition;
pub mod identifier;
pub mod schema;

pub use composition::{compose, ChainLayer, ComposedChain};
pub use identifier::{parse_instance, PluginInstance};

/// A resolved credential handle: only the `cred://` reference ever crosses the
/// plugin boundary, never the material behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    /// The `cred://` reference the configuration carried.
    pub reference: String,
    /// Opaque handle to the resolved material. Foundation code never inspects
    /// it, and releasing it zeroes the underlying buffer (owned by entry 2.6).
    pub handle: String,
}

/// The downstream caller identity the auth phase reads and writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Principal {
    pub subject_id: Option<Uuid>,
    pub tenant_id: Option<Uuid>,
    pub scopes: Vec<String>,
}

/// `AuthContext` — the single auth plugin's payload (ADR 0008).
///
/// ADR 0008 `AuthPlugin.authenticate` mutates the *outbound request* rather
/// than the inbound one, so the credential injection surface of the payload is
/// `outbound_headers`: the `Authorization` header an OAuth2 plugin writes and
/// the `x-api-key` header / query parameter an API-key plugin writes never
/// appear in the inbound request headers this context is built from.
#[derive(Debug, Clone, Default)]
pub struct AuthContext {
    /// Effective plugin configuration, already merged.
    pub config: Option<serde_json::Value>,
    /// Credential references found in `config`, resolved at request time.
    pub credentials: Vec<ResolvedCredential>,
    /// The principal the plugin authenticated.
    pub principal: Principal,
    /// The headers the auth plugin injects into the outbound request.
    pub outbound_headers: Vec<(String, String)>,
    /// Query parameters the auth plugin injects into the outbound request.
    pub outbound_query: Vec<(String, String)>,
}

/// `RequestContext` — the guard and request-transform payload.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    /// Guard decisions and transforms may attach values for later plugins.
    pub extensions: Vec<(String, String)>,
    /// The effective configuration of the plugin being run, so a guard reads
    /// `required_request_headers` from its own binding and not from a
    /// neighbouring one (ADR 0009).
    pub config: Option<serde_json::Value>,
}

/// `ResponseContext` — the response-transform payload.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub extensions: Vec<(String, String)>,
    /// The effective configuration of the plugin being run, the response-phase
    /// counterpart of [`RequestContext::config`] (ADR 0009).
    pub config: Option<serde_json::Value>,
}

/// `ErrorContext` — the error-transform payload.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub error: Option<DomainError>,
    pub status: u16,
}

/// A guard verdict: the first reject is terminal and no upstream call is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Reject { status: u16, reason: String },
}

impl GuardDecision {
    /// An allow verdict.
    pub const fn allow() -> Self {
        Self::Allow
    }

    /// Whether the verdict admits the request.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The error type ADR 0008 types the plugin methods with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// The plugin rejected the request (auth failure, guard rejection).
    ///
    /// `status` is the phase-specific status the plugin produced when it
    /// produced one, and is what `inst-ps-exec-17` preserves: a guard that
    /// reports an error rather than a reject decision keeps its status when it
    /// carried one and is mapped as a guard reject otherwise.
    #[error("{reason}")]
    Rejected {
        reason: String,
        status: Option<u16>,
    },
    /// The plugin could not reach its backing service.
    #[error("plugin backing service unavailable")]
    Unavailable,
    /// The plugin itself failed; rendered `500`, nothing cached.
    #[error("{0}")]
    Internal(String),
}

impl PluginError {
    /// A rejection with no phase-specific status.
    #[must_use]
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self::Rejected { reason: reason.into(), status: None }
    }

    /// A rejection carrying its phase-specific status.
    #[must_use]
    pub fn rejected_with(reason: impl Into<String>, status: u16) -> Self {
        Self::Rejected { reason: reason.into(), status: Some(status) }
    }

    /// The phase-specific status the rejection carried, if any.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Rejected { status, .. } => *status,
            _ => None,
        }
    }
}

impl From<DomainError> for PluginError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::SecretNotFound { .. } => Self::Unavailable,
            DomainError::AuthenticationFailed { .. } => Self::rejected("authentication failed"),
            other => Self::Internal(other.to_string()),
        }
    }
}

/// The single authentication plugin of a request. At most one runs per
/// request, exactly once, before any guard.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The registry key: a built-in identifier or a custom plugin UUID.
    fn id(&self) -> &str;
    /// The concrete GTS plugin type identifier.
    fn plugin_type(&self) -> &str;
    /// Authenticate the request; exactly one auth plugin runs, before any
    /// guard.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when authentication fails.
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError>;
}

/// A guard plugin. Guard rejections are terminal: no upstream call is made.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;

    /// Guard the request; the first reject is terminal.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard cannot run.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Guard the response after the upstream call, before returning.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard cannot run.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// A transform plugin. Transforms run in chain order against the request, the
/// response, and the error context.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;

    /// Transform the request before the upstream call.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot run.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Transform the response after a successful upstream call.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot run.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Transform the error context on a failed upstream call.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot run.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// A composed chain reference resolved against the registries before
/// execution; an unresolvable reference fails the request with
/// [`DomainError::PluginNotFound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}
