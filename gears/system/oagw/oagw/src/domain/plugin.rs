//! Plugin trait definitions (DESIGN §3.2 Plugin System, ADR 0002).
//!
//! Three async plugin categories with deterministic execution order:
//!
//! `Auth → Guards → Transform(request) → Upstream → Transform(response/error)`
//!
//! Plugin config is passed as an argument at invocation; secrets are resolved
//! through a [`SecretResolver`] adapter so plugins never talk to the
//! credential store directly.

use async_trait::async_trait;
use http::{HeaderMap, Method, StatusCode};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::PluginKind;

/// Resolves secret material by reference (e.g. `cred://...`) for a tenant.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolve the raw secret bytes for `secret_ref`.
    ///
    /// # Errors
    ///
    /// - `DomainError::SecretNotFound` when the credential store has no
    ///   matching secret (or the tenant cannot access it).
    /// - `DomainError::Internal` on store transport failure.
    async fn resolve(&self, tenant_id: Uuid, secret_ref: &str) -> Result<Vec<u8>, DomainError>;
}

/// Plugin-visible view of the inbound proxy request.
#[derive(Debug, Clone)]
pub struct ProxyRequestView {
    pub method: Method,
    /// Absolute path (including `/path_suffix` appended by route matching).
    pub path: String,
    /// Raw query string (undecoded).
    pub query: String,
    pub headers: HeaderMap,
    pub tenant_id: Uuid,
    /// Declared `Content-Length` when present.
    pub body_length_hint: Option<u64>,
}

/// Plugin-visible view of the upstream response.
#[derive(Debug, Clone)]
pub struct ProxyResponseView {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Buffer size in bytes (full body available during transform).
    pub body_len: usize,
}

/// Result of a guard decision.
#[derive(Debug, Clone)]
pub struct GuardRejection {
    /// HTTP status to return to the client.
    pub status: u16,
    /// GTS error instance id.
    pub gts_type: &'static str,
    /// Starlark-free static title.
    pub title: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

/// Plugin execution error.
#[derive(Debug)]
pub enum PluginError {
    /// The plugin could not be configured/instantiated → 400.
    InvalidConfig(String),
    /// The request/response was rejected by a guard → mapped to its declared
    /// status/type.
    Rejected(GuardRejection),
    /// External dependency failure (auth endpoint, secret store) → 500/503.
    External(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(m) => write!(f, "plugin config error: {m}"),
            Self::Rejected(r) => write!(f, "guard rejected: {}", r.detail),
            Self::External(m) => write!(f, "plugin external error: {m}"),
        }
    }
}

impl std::error::Error for PluginError {}

impl PluginError {
    /// Convenience constructor for guard rejections.
    #[must_use]
    pub fn rejected(status: u16, gts_type: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self::Rejected(GuardRejection {
            status,
            gts_type,
            title,
            detail: detail.into(),
        })
    }
}

/// Auth plugin: injects credentials into the outbound request.
///
/// Exactly one auth plugin per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Canonical identifier (e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    fn id(&self) -> &'static str;

    /// Inject credentials (headers, query params, token) into `request`.
    ///
    /// # Errors
    ///
    /// - `PluginError::InvalidConfig` when `config` is malformed.
    /// - `PluginError::Rejected` when authentication cannot be performed
    ///   (maps to `401 AuthenticationFailed`).
    /// - `PluginError::External` on secret-store/network failure.
    async fn inject_credentials(
        &self,
        request: &mut ProxyRequestView,
        config: &serde_json::Value,
        secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError>;
}

/// Guard plugin: validates requests/responses and can reject them.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Canonical identifier.
    fn id(&self) -> &'static str;

    /// Whether this guard inspects requests.
    fn checks_request(&self) -> bool {
        true
    }

    /// Whether this guard inspects responses.
    fn checks_response(&self) -> bool {
        false
    }

    /// Reject or accept the inbound request.
    ///
    /// # Errors
    ///
    /// `PluginError::Rejected` rejects the request; `InvalidConfig` → 400;
    /// `External` → 500/502.
    async fn check_request(
        &self,
        request: &ProxyRequestView,
        config: &serde_json::Value,
    ) -> Result<(), PluginError>;

    /// Reject or accept the upstream response (default: accept).
    ///
    /// # Errors
    ///
    /// `PluginError::Rejected` replaces the response with a gateway error.
    async fn check_response(
        &self,
        _response: &ProxyResponseView,
        _config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Transform plugin: mutates requests, responses and gateway errors.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Canonical identifier.
    fn id(&self) -> &'static str;

    /// Mutate the outbound request before it is sent upstream.
    ///
    /// # Errors
    ///
    /// `PluginError::InvalidConfig` → 400; `External` → 502.
    async fn on_request(
        &self,
        request: &mut ProxyRequestView,
        config: &serde_json::Value,
    ) -> Result<(), PluginError>;

    /// Mutate the upstream response before it is returned to the client.
    ///
    /// # Errors
    ///
    /// `PluginError::InvalidConfig` → 400; `External` → 502.
    async fn on_response(
        &self,
        response: &mut ProxyResponseView,
        config: &serde_json::Value,
    ) -> Result<(), PluginError>;

    /// Mutate a gateway error before it is emitted (e.g. attach a header).
    async fn on_error(
        &self,
        _error: &mut crate::domain::error::ProblemSpec,
        _config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Marker for the plugin category.
#[must_use]
pub fn kind_of(id: &str) -> PluginKind {
    PluginKind::from_gts_id(id).unwrap_or(PluginKind::Auth)
}

/// Convenience: build a rejected error for `AuthenticationFailed` (401).
#[must_use]
pub fn auth_failed(detail: impl Into<String>) -> PluginError {
    PluginError::rejected(
        401,
        crate::domain::gts::ERR_AUTH_FAILED,
        "Authentication Failed",
        detail,
    )
}

/// Convenience: build a rejected error for `ValidationError` (400).
#[must_use]
pub fn validation_rejected(detail: impl Into<String>) -> PluginError {
    PluginError::rejected(
        400,
        crate::domain::gts::ERR_VALIDATION,
        "Validation Error",
        detail,
    )
}

/// `DomainError` used for plugin secret failures so `?` works in callers that
/// already operate on `DomainError`.
impl From<PluginError> for crate::domain::error::ProblemSpec {
    fn from(value: PluginError) -> Self {
        match value {
            PluginError::InvalidConfig(msg) => DomainError::validation(msg).to_problem(),
            PluginError::Rejected(r) => crate::domain::error::ProblemSpec {
                gts_type: r.gts_type,
                status: r.status,
                title: r.title,
                detail: r.detail,
                context: Vec::new(),
                retry_after_seconds: None,
            },
            PluginError::External(msg) => DomainError::Internal(format!("plugin error: {msg}")).to_problem(),
        }
    }
}
