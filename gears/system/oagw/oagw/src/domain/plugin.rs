//! Plugin identity, the built-in catalog, and the plugin contracts.
//!
//! A *plugin reference* is a GTS id. Two families exist:
//!
//! * **built-in** references are catalog-only single-type ids such as
//!   `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`; they
//!   resolve against the compile-time catalog in this module.
//! * **custom** references carry a UUID tail —
//!   `gts.cf.core.oagw.transform_plugin.v1~3f2c…` — and resolve against the
//!   plugin rows of the calling tenant.
//!
//! Both the upstream and the route chain bind the same two families — guards
//! and transforms (`DESIGN` §3.2). Outbound `auth` is *not* a chain entry: it
//! is a dedicated upstream field (`auth.type`), validated by
//! [`crate::domain::validation::validate_auth`].
//!
//! The set of *bindable* references is narrower than the catalog: several
//! built-ins exist so manifests and diagnostics can name them, but a chain
//! that binds them is refused.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::domain::model::{GUARD_PLUGIN_TYPE, PluginType, TRANSFORM_PLUGIN_TYPE};

/// Which chain a plugin binds to.
///
/// Today both chains accept the same plugin families, so the value only shows
/// up in diagnostics; it is kept because the data plane merges upstream and
/// route chains differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainKind {
    /// The upstream-level chain.
    Upstream,
    /// The route-level chain.
    Route,
}

impl fmt::Display for ChainKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream => f.write_str("upstream"),
            Self::Route => f.write_str("route"),
        }
    }
}

/// The GTS base types a chain may bind (`DESIGN` §3.2).
///
/// Both chains bind guards and transforms; `auth` is never a chain entry.
pub const CHAIN_BASE_TYPES: &[&str] = &[GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE];

/// The plugin kinds a chain may bind, the row-side mirror of
/// [`CHAIN_BASE_TYPES`].
pub const CHAIN_ROW_TYPES: &[PluginType] = &[PluginType::Guard, PluginType::Transform];

/// `true` when a custom plugin row of this kind may bind to a chain.
#[must_use]
pub fn allows_row(plugin_type: PluginType) -> bool {
    CHAIN_ROW_TYPES.contains(&plugin_type)
}

/// `true` when `reference` carries a UUID tail (a custom plugin row).
#[must_use]
pub fn has_uuid_tail(reference: &str) -> bool {
    uuid_tail(reference).is_some()
}

/// Extract the UUID tail of a plugin reference, if any.
///
/// A UUID is recognized by its dash layout (8-4-4-4-12) at the tail of the id;
/// that keeps `cf.core.oagw.noop.v1` a built-in and `3f2c…` a row.
#[must_use]
pub fn uuid_tail(reference: &str) -> Option<uuid::Uuid> {
    let tail = reference.rsplit('~').next().unwrap_or(reference);
    if tail.len() != 36 || tail.chars().filter(|c| *c == '-').count() != 4 {
        return None;
    }
    let bytes = tail.as_bytes();
    for index in [8usize, 13, 18, 23] {
        if bytes[index] != b'-' {
            return None;
        }
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    {
        return None;
    }
    uuid::Uuid::parse_str(tail).ok()
}

/// The built-in plugin catalog of this gear.
///
/// `bindable == false` marks catalog-only entries: they exist so manifests and
/// diagnostics can name them, but a chain that binds them is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinPlugin {
    /// GTS id of the plugin implementation.
    pub gts_id: &'static str,
    /// Human-readable name used in diagnostics.
    pub name: &'static str,
    /// `true` when the plugin may be bound to a chain today.
    pub bindable: bool,
}

/// Built-in auth plugins, addressable through `auth.type` only.
///
/// `basic.v1` and `bearer.v1` are reserved identifiers cataloged in the
/// types-registry with no backing [`AuthPlugin`] implementation, so naming
/// either fails with `unknown auth plugin` (`DESIGN` §3.2).
pub const AUTH_BUILTINS: &[BuiltinPlugin] = &[
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
        name: "noop",
        bindable: true,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        name: "apikey",
        bindable: true,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
        name: "oauth2_client_cred",
        bindable: true,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
        name: "oauth2_client_cred_basic",
        bindable: true,
    },
    // Catalog-only: credential-backed plugins land with the secret slice.
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        name: "basic",
        bindable: false,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        name: "bearer",
        bindable: false,
    },
];

/// Built-in guard plugins.
///
/// `required_headers` is the only resolvable guard; `timeout` and `cors` exist
/// for types-registry cataloging and cannot be bound through `plugins.items`
/// (`DESIGN` §3.2).
pub const GUARD_BUILTINS: &[BuiltinPlugin] = &[
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        name: "required_headers",
        bindable: true,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        name: "timeout",
        bindable: false,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        name: "cors",
        bindable: false,
    },
];

/// Built-in transform plugins.
///
/// `logging` and `metrics` are catalog-only ids: the gateway emits its own
/// logs and metrics, so neither is resolvable from a chain (`DESIGN` §3.2).
pub const TRANSFORM_BUILTINS: &[BuiltinPlugin] = &[
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        name: "request_id",
        bindable: true,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        name: "logging",
        bindable: false,
    },
    BuiltinPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
        name: "metrics",
        bindable: false,
    },
];

/// Every built-in a chain may bind: guards and transforms.
pub fn chain_builtins() -> impl Iterator<Item = &'static BuiltinPlugin> {
    GUARD_BUILTINS.iter().chain(TRANSFORM_BUILTINS.iter())
}

/// `true` when `reference` names a built-in plugin that a chain may bind.
#[must_use]
pub fn is_bindable_builtin(reference: &str) -> bool {
    chain_builtins().any(|entry| entry.bindable && entry.gts_id == reference)
}

/// `true` when `reference` may be bound to `kind`: either a bindable built-in
/// or a custom row of an accepted base type.
///
/// A bare UUID is accepted on an upstream chain only: `upstream.v1` types
/// `plugins.items` as `oneOf [gts-identifier, uuid]` while `route.v1` accepts
/// the gts-identifier form alone, so a route binding must carry its base type.
#[must_use]
pub fn is_bindable(kind: ChainKind, reference: &str) -> bool {
    if is_bindable_builtin(reference) {
        return true;
    }
    // `auth.type` is the only address for the auth plugins: a chain entry that
    // carries the auth base type is refused whatever its tail.
    if reference.starts_with(crate::domain::model::AUTH_PLUGIN_TYPE) {
        return false;
    }
    if uuid_tail(reference).is_none() {
        return false;
    }
    if CHAIN_BASE_TYPES
        .iter()
        .any(|base| reference.starts_with(base))
    {
        return true;
    }
    // A UUID tail with no base type, which only the upstream schema accepts.
    kind == ChainKind::Upstream
}

/// Result of a guard plugin evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The request may proceed.
    Allow,
    /// The request is refused with a detail string.
    Deny(String),
}

impl GuardDecision {
    /// The permissive decision.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }
}

/// Outcome of one auth plugin invocation.
///
/// Kept for the data-plane slice: it is what `authenticate` returns to the
/// proxy engine, which forwards `forwarded_headers` upstream.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthOutcome {
    /// Principal identity attributed to the caller, when any.
    pub subject: Option<String>,
    /// Credentials to forward upstream instead of the caller's own.
    pub forwarded_headers: BTreeMap<String, String>,
}

/// Response-side context handed to a guard or transform plugin.
///
/// Data-plane type: the control plane never builds one, and the proxy engine
/// populates it in the data-plane slice.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// Upstream response status.
    pub status: u16,
    /// Upstream response headers, lowercased names.
    pub headers: BTreeMap<String, String>,
    /// Correlation id propagated by the platform.
    pub trace_id: Option<String>,
}

/// Error-side context handed to [`TransformPlugin::transform_error`].
///
/// Data-plane type: the control plane never builds one, and the proxy engine
/// populates it in the data-plane slice.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ErrorContext {
    /// The error the gateway is about to emit.
    pub error: crate::domain::error::DomainError,
    /// Correlation id propagated by the platform.
    pub trace_id: Option<String>,
}

/// Contract implemented by auth plugins (`ADR`-0002).
///
/// The data plane resolves these from the Starlark source in a later slice;
/// the control plane only stores and references them. Named plugins are
/// resolved from an in-process registry, UUID-backed ones from a
/// `oagw_plugin` row.
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Stable GTS id of the plugin instance.
    fn gts_id(&self) -> String;
    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &str {
        crate::domain::model::AUTH_PLUGIN_TYPE
    }
    /// Authenticate a request, attributing a principal and the credentials to
    /// forward upstream.
    async fn authenticate(
        &self,
        request: &mut crate::domain::dto::ProxyContext,
    ) -> Result<AuthOutcome, crate::domain::error::DomainError>;
}

/// Contract implemented by guard plugins (`ADR`-0002, `ADR`-0009).
#[async_trait::async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Stable GTS id of the plugin instance.
    fn gts_id(&self) -> String;
    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE
    }
    /// Evaluate the guard against a request.
    async fn guard_request(
        &self,
        request: &crate::domain::dto::ProxyContext,
    ) -> Result<GuardDecision, crate::domain::error::DomainError>;
    /// Evaluate the guard against an upstream response. Defaults to allowing:
    /// most guards are request-side only.
    async fn guard_response(
        &self,
        _response: &ResponseContext,
    ) -> Result<GuardDecision, crate::domain::error::DomainError> {
        Ok(GuardDecision::allow())
    }
}

/// Contract implemented by transform plugins (`ADR`-0002).
#[async_trait::async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Stable GTS id of the plugin instance.
    fn gts_id(&self) -> String;
    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &str {
        TRANSFORM_PLUGIN_TYPE
    }
    /// Rewrite the request before it leaves the gateway.
    async fn transform_request(
        &self,
        request: &mut crate::domain::dto::ProxyContext,
    ) -> Result<(), crate::domain::error::DomainError>;
    /// Rewrite the upstream response before it reaches the caller. Defaults to
    /// a no-op: most transforms are request-side only.
    async fn transform_response(
        &self,
        _response: &mut ResponseContext,
    ) -> Result<(), crate::domain::error::DomainError> {
        Ok(())
    }
    /// Observe (and possibly replace) an error the gateway is about to emit.
    /// Defaults to a no-op.
    async fn transform_error(
        &self,
        _error: &mut ErrorContext,
    ) -> Result<(), crate::domain::error::DomainError> {
        Ok(())
    }
}

/// A compiled plugin instance held by the runtime.
///
/// The proxy engine dispatches on this: `Auth` runs before the upstream call,
/// `Guard` may refuse either side, and `Transform` may rewrite either side or
/// the emitted error.
#[allow(dead_code)]
pub enum SharedPlugin {
    /// Credential injection; one per upstream.
    Auth(Arc<dyn AuthPlugin>),
    /// Validation and policy enforcement; may refuse.
    Guard(Arc<dyn GuardPlugin>),
    /// Request/response mutation.
    Transform(Arc<dyn TransformPlugin>),
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugin_tests.rs"]
mod tests;
