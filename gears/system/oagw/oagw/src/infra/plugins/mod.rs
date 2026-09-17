//! Plugin framework of the OAGW data plane
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md)).
//!
//! [`registry`] owns the in-process registry, the built-in catalog of the PRD's
//! GTS identifiers and the custom (tenant-defined) plugin catalog; [`auth`],
//! [`guard`] and [`transform`] implement the three `#[async_trait]` plugin
//! traits and the built-ins that back them. Credential resolution is a port:
//! [`SecretResolver`] is implemented against `cred_store` by
//! [`CredStoreSecretResolver`](crate::infra::secrets::CredStoreSecretResolver),
//! so the plugins take a resolver handle instead of touching credstore
//! directly — the gear falls back to a fail-closed resolver when no credential
//! store is configured.
//!
//! Execution order is Auth → Guards → Transform(request) → upstream →
//! Transform(response/error), with upstream-bound plugins running before
//! route-bound ones. [`execution::PluginExecution`] is the engine the data plane
//! drives and the only place that order is decided, so the proxy cannot compose
//! it differently; [`PluginChain`] is the phase-3 chain it supersedes on the
//! response path, where guards now run before the response transforms.
//!
//! Plugin state is immutable: built-ins are shared `Send + Sync` singletons and
//! the per-binding configuration travels on the [`RequestContext`] instead of
//! living inside a plugin instance.

use std::collections::BTreeMap;

use async_trait::async_trait;
use credstore_sdk::{SecretRef, SecretValue};
use http::{HeaderMap, StatusCode};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::OagwError;

pub mod auth;
pub mod execution;
pub mod guard;
pub mod registry;
pub mod transform;

pub use auth::{AuthPlugin, TokenCacheConfig};
pub use execution::{ExecutionOutcome, PluginExecution, UpstreamCall};
pub use guard::GuardPlugin;
pub use registry::{
    BuiltinCatalog, BuiltinDescriptor, PluginCatalog, PluginChain, PluginDefinition, PluginInput,
    PluginInstance, PluginRegistry, PluginUsage, UnavailableSecretResolver,
};
pub use transform::TransformPlugin;

// ---------------------------------------------------------------------------
// Plugin vocabulary
// ---------------------------------------------------------------------------

/// The three plugin kinds of the PRD catalog (`plugin_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginType {
    /// Credential injection; one per upstream (`auth.type`).
    Auth,
    /// Validation and policy enforcement; may reject.
    Guard,
    /// Request/response/error mutation.
    Transform,
}

impl PluginType {
    /// Every plugin kind, in catalog order.
    pub const ALL: [Self; 3] = [Self::Auth, Self::Guard, Self::Transform];

    /// The wire name of the kind, as used by `plugin_type` and
    /// `$filter=type eq '...'`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The GTS base type identifiers of this kind resolve under, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~`.
    #[must_use]
    pub const fn base_type(self) -> &'static str {
        match self {
            Self::Auth => crate::infra::plugins::registry::AUTH_PLUGIN_BASE_TYPE,
            Self::Guard => crate::infra::plugins::registry::GUARD_PLUGIN_BASE_TYPE,
            Self::Transform => crate::infra::plugins::registry::TRANSFORM_PLUGIN_BASE_TYPE,
        }
    }
}

impl std::fmt::Display for PluginType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PluginType {
    type Err = OagwError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auth" => Ok(Self::Auth),
            "guard" => Ok(Self::Guard),
            "transform" => Ok(Self::Transform),
            _other => Err(OagwError::Validation {
                message: format!(
                    "plugin_type '{value}' is not a known plugin type: expected 'auth', 'guard' \
                     or 'transform'"
                ),
            }),
        }
    }
}

/// The decision of a guard plugin for one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The request or response passes the check.
    Allow,
    /// Reject with a phase-specific status, a machine-readable code and a
    /// client-safe description.
    Reject {
        /// HTTP status of the rejection: 400 in the request phase, 502 in the
        /// response phase.
        status: StatusCode,
        /// Machine-readable rejection reason, e.g. `REQUIRED_HEADER_MISSING`.
        error_code: String,
        /// Human-readable, client-safe description of the rejection.
        message: String,
    },
}

impl GuardDecision {
    /// `true` when the decision lets the request or response through.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

// ---------------------------------------------------------------------------
// Per-request state
// ---------------------------------------------------------------------------

/// The state one request carries through the plugin chain.
///
/// A plugin is immutable and shared between concurrent requests, so everything
/// that varies per request — including the configuration of the binding
/// currently being executed — lives here.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Tenant the request was authenticated against.
    pub tenant_id: Uuid,
    /// Upstream the request resolved to.
    pub upstream_id: Uuid,
    /// Route the request matched, when one did.
    pub route_id: Option<Uuid>,
    /// Authenticated caller.
    pub security: SecurityContext,
    /// Full GTS identifier of the plugin currently executing.
    pub plugin_ref: String,
    /// `config` object of the binding currently executing.
    pub config: Option<Value>,
    /// Headers of the request about to be sent upstream.
    pub request_headers: HeaderMap,
    /// Path of the request, without the query string.
    pub path: String,
    /// Query string of the request, without the leading `?`.
    pub query: Option<String>,
    /// Cross-plugin scratch space; auth plugins publish their resolved
    /// credential material here for the later phases of the chain.
    pub attributes: BTreeMap<String, String>,
}

impl RequestContext {
    /// Builds a request context for `security` calling `upstream_id`.
    #[must_use]
    pub fn new(security: SecurityContext, upstream_id: Uuid, path: &str) -> Self {
        Self {
            tenant_id: security.subject_tenant_id(),
            upstream_id,
            route_id: None,
            security,
            plugin_ref: String::new(),
            config: None,
            request_headers: HeaderMap::new(),
            path: path.to_owned(),
            query: None,
            attributes: BTreeMap::new(),
        }
    }

    /// The value of `key` in the scratch space, if a plugin set it.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// Records `value` under `key` in the scratch space.
    pub fn set_attribute(&mut self, key: &str, value: impl Into<String>) {
        self.attributes.insert(key.to_owned(), value.into());
    }
}

/// The upstream's response as the plugin chain sees it.
#[derive(Debug, Clone)]
pub struct UpstreamResponseView {
    /// Status the upstream returned.
    pub status: StatusCode,
    /// Headers the upstream returned.
    pub headers: HeaderMap,
    /// Body the upstream returned, as bytes.
    pub body: Vec<u8>,
}

impl UpstreamResponseView {
    /// Builds a response view with no headers and an empty body.
    #[must_use]
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }
}

/// The error the proxy is about to return, as the plugin chain sees it.
#[derive(Debug, Clone)]
pub struct ErrorView {
    /// HTTP status the error maps to.
    pub status: StatusCode,
    /// Machine-readable error code, e.g. `REQUIRED_HEADER_MISSING`.
    pub error_code: String,
    /// Human-readable, client-safe description.
    pub message: String,
    /// Headers to set on the error response.
    pub headers: HeaderMap,
}

// ---------------------------------------------------------------------------
// Credential resolution port
// ---------------------------------------------------------------------------

/// Resolves a `cred://` secret reference on behalf of an auth plugin
/// ([DESIGN.md](../../../../docs/DESIGN.md) "Secret Access Control").
///
/// The plugin never sees *how* a reference is resolved: it names the reference
/// and the resolver decides whether the calling tenant may read it.
/// [`CredStoreSecretResolver`](crate::infra::secrets::CredStoreSecretResolver)
/// is the production implementation and
/// [`UnavailableSecretResolver`](registry::UnavailableSecretResolver) fails
/// closed when the gear has no credential store.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves `secret_ref` for the caller of `ctx`.
    ///
    /// # Errors
    /// [`OagwError::SecretNotFound`] when the reference is not resolvable for
    /// the caller, [`OagwError::SecretError`] when the store cannot be
    /// consulted. The reference itself never appears in the message: it is
    /// operator data, not client data.
    async fn resolve(
        &self,
        ctx: &RequestContext,
        secret_ref: &SecretRef,
    ) -> Result<SecretValue, OagwError>;

    /// Resolves a reference as spelled by a binding, stripping an optional
    /// `cred://` prefix first.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the reference is not well-formed, and
    /// whatever [`Self::resolve`] reports otherwise.
    async fn resolve_cref(
        &self,
        ctx: &RequestContext,
        raw: &str,
    ) -> Result<SecretValue, OagwError> {
        let secret_ref = crate::infra::secrets::normalize_secret_ref(raw)?;
        self.resolve(ctx, &secret_ref).await
    }
}
