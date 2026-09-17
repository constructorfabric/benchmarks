//! Plugin traits (ADR 0002 "Plugin System").
//!
//! Three plugin types, three traits, deterministic execution order:
//!
//! ```text
//! Auth → Guards → Transform(on_request) → upstream call → Transform(on_response / on_error)
//! ```
//!
//! The same traits are implemented by built-in plugins ([`crate::infra::plugin`])
//! and by external gears, so no special-casing exists between the two.
//!
//! The context types below are the *minimum* surface the traits need; the data
//! plane (part 2, `infra/proxy/`) owns the concrete request/response plumbing
//! and populates them.

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Error type returned by plugin implementations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// The plugin is not implemented in this build (part-2 work).
    #[error("plugin `{plugin_id}` is not implemented in this build")]
    Unimplemented {
        /// Plugin identifier (GTS instance id).
        plugin_id: String,
    },
    /// The plugin failed while executing.
    #[error("plugin `{plugin_id}` failed: {detail}")]
    Internal {
        /// Plugin identifier (GTS instance id).
        plugin_id: String,
        /// Human-readable explanation.
        detail: String,
    },
    /// The plugin could not read the secret material it needs.
    #[error("plugin `{plugin_id}` could not read its secret: {detail}")]
    SecretUnavailable {
        /// Plugin identifier (GTS instance id).
        plugin_id: String,
        /// Human-readable explanation.
        detail: String,
    },
}

impl PluginError {
    /// Convenience constructor for [`PluginError::Unimplemented`].
    #[must_use]
    pub fn unimplemented(plugin_id: impl Into<String>) -> Self {
        Self::Unimplemented {
            plugin_id: plugin_id.into(),
        }
    }
}

impl From<PluginError> for DomainError {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Unimplemented { plugin_id } => DomainError::PluginUnavailable {
                detail: format!("plugin `{plugin_id}` is not implemented in this build"),
            },
            PluginError::Internal { plugin_id, detail } => DomainError::Internal {
                diagnostic: format!("plugin `{plugin_id}` failed: {detail}"),
            },
            PluginError::SecretUnavailable { plugin_id, detail } => DomainError::SecretNotFound {
                detail: format!("plugin `{plugin_id}`: {detail}"),
            },
        }
    }
}

/// `Result` alias used by every plugin method.
pub type PluginResult<T> = Result<T, PluginError>;

/// Identity + resolved configuration handed to a plugin invocation.
///
/// `config` is the merged plugin configuration (the binding's `config`
/// overlaid on the plugin resource's default `config`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginConfig {
    /// GTS instance id of the executing plugin.
    pub plugin_id: String,
    /// Position of the plugin in the executed chain (0-based).
    pub position: usize,
    /// Whether the plugin was bound at upstream level (as opposed to route).
    pub at_upstream_level: bool,
    /// Merged plugin configuration.
    pub config: serde_json::Value,
}

impl PluginConfig {
    /// Read a string key from the merged configuration.
    #[must_use]
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(|v| v.as_str())
    }
    /// Read a comma-separated header list (`required_request_headers` in
    /// ADR 0009): split, trim, lower-case, drop empty entries.
    #[must_use]
    pub fn header_list(&self, key: &str) -> Vec<String> {
        self.str_field(key)
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Everything a plugin may observe and (for auth/transform) mutate about the
/// outbound request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// HTTP method of the downstream request.
    pub method: String,
    /// Path sent upstream (after any rewrite).
    pub path: String,
    /// Raw query string (without the leading `?`).
    pub query: String,
    /// Headers of the outbound request.
    pub headers: HeaderMap<HeaderValue>,
    /// Buffered request body, when the plugin phase needs it.
    pub body: Option<Bytes>,
    /// Request headers as received from the client.
    pub downstream_headers: HeaderMap<HeaderValue>,
    /// The caller's security context (used to resolve credstore secrets for
    /// credential-injection plugins).
    pub security: SecurityContext,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Resolved upstream.
    pub upstream_id: Option<Uuid>,
    /// Matched route.
    pub route_id: Option<Uuid>,
    /// Resolved upstream alias.
    pub alias: Option<String>,
    /// Trace / request id for logs and problem instances.
    pub trace_id: Option<String>,
    /// Merged plugin configuration.
    pub config: PluginConfig,
    /// Free-form scratch space plugins may use to hand state to later phases.
    pub attributes: BTreeMap<String, String>,
}

impl RequestContext {
    /// Case-insensitive header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// True when every named header is present (case-insensitive).
    #[must_use]
    pub fn has_all_headers(&self, names: &[String]) -> bool {
        names.iter().all(|n| self.headers.contains_key(n.as_str()))
    }

    /// Insert (or replace) a header value.
    ///
    /// # Errors
    /// Returns [`PluginError::Internal`] when the header name is not a valid
    /// `HeaderName` or the value is not visible-ASCII.
    pub fn set_header(&mut self, name: &str, value: &str) -> PluginResult<()> {
        let parsed = name
            .parse::<http::HeaderName>()
            .map_err(|e| PluginError::Internal {
                plugin_id: self.config.plugin_id.clone(),
                detail: format!("invalid header name `{name}`: {e}"),
            })?;
        let parsed_value = HeaderValue::from_str(value).map_err(|e| PluginError::Internal {
            plugin_id: self.config.plugin_id.clone(),
            detail: format!("invalid header value for `{name}`: {e}"),
        })?;
        self.headers.insert(parsed, parsed_value);
        Ok(())
    }
}

/// Everything a plugin may observe about the upstream response.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: u16,
    /// Headers of the upstream response.
    pub headers: HeaderMap<HeaderValue>,
    /// Buffered response body, when the data plane buffers it.
    pub body: Option<Bytes>,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Resolved upstream.
    pub upstream_id: Option<Uuid>,
    /// Matched route.
    pub route_id: Option<Uuid>,
    /// Trace / request id.
    pub trace_id: Option<String>,
    /// Merged plugin configuration.
    pub config: PluginConfig,
    /// Request-side scratch space, shared with the request phase.
    pub attributes: BTreeMap<String, String>,
}

impl ResponseContext {
    /// True when every named header is present (case-insensitive).
    #[must_use]
    pub fn has_all_headers(&self, names: &[String]) -> bool {
        names.iter().all(|n| self.headers.contains_key(n.as_str()))
    }
}

/// Everything a plugin may observe about a failed exchange.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// The domain error that caused the failure.
    pub error: DomainError,
    /// Status that will be returned downstream.
    pub status: u16,
    /// Headers of the (possibly synthesised) response.
    pub headers: HeaderMap<HeaderValue>,
    /// Buffered error body, when one was produced.
    pub body: Option<Bytes>,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Resolved upstream, when one was resolved.
    pub upstream_id: Option<Uuid>,
    /// Trace / request id.
    pub trace_id: Option<String>,
    /// Merged plugin configuration.
    pub config: PluginConfig,
    /// Request-side scratch space, shared with the request phase.
    pub attributes: BTreeMap<String, String>,
}

impl ErrorContext {
    /// Status of the failure, for convenience.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }
}

/// Verdict of a guard plugin phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Reject the request with a phase-specific status.
    Reject {
        /// HTTP status to return (400 in the request phase, 502 in the
        /// response phase for ADR 0009).
        status: u16,
        /// Machine-readable error code (e.g. `REQUIRED_HEADER_MISSING`).
        error_code: &'static str,
        /// Human-readable explanation.
        detail: String,
    },
}

impl GuardDecision {
    /// An allow verdict.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }

    /// A reject verdict.
    #[must_use]
    pub fn reject(status: u16, error_code: &'static str, detail: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code,
            detail: detail.into(),
        }
    }

    /// The rejection status, or `None` when the verdict is an allow.
    #[must_use]
    pub const fn rejection_status(&self) -> Option<u16> {
        match self {
            Self::Allow => None,
            Self::Reject { status, .. } => Some(*status),
        }
    }
}

/// Credential injection (`gts.cf.core.oagw.auth_plugin.v1~*`).
///
/// Executed once per request, before guards. One auth plugin per upstream.
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Registry key: the plugin's full GTS instance id.
    fn id(&self) -> &str;
    /// The GTS instance id this plugin implements (same as [`AuthPlugin::id`]
    /// for built-ins).
    fn plugin_type(&self) -> &str;

    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    /// [`PluginError`] when the credentials cannot be resolved or injected.
    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()>;
}

/// Validation / policy enforcement (`gts.cf.core.oagw.guard_plugin.v1~*`).
///
/// Executed after auth, before transform; multiple guards may be bound.
#[async_trait::async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Registry key: the plugin's full GTS instance id.
    fn id(&self) -> &str;
    /// The GTS instance id this plugin implements.
    fn plugin_type(&self) -> &str;

    /// Validate the outbound request before it is proxied.
    ///
    /// # Errors
    /// [`PluginError`] when the plugin itself fails (as opposed to rejecting).
    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision>;

    /// Validate the upstream response before it is returned to the client.
    ///
    /// # Errors
    /// [`PluginError`] when the plugin itself fails.
    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision>;
}

/// Phases a transform plugin participates in.
#[allow(clippy::enum_variant_names)] // the On* names are the spec's own vocabulary
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformPhase {
    /// Before the upstream call.
    OnRequest,
    /// After the upstream responded.
    OnResponse,
    /// Instead of the normal response, on failure.
    OnError,
}

impl TransformPhase {
    /// Wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on_request",
            Self::OnResponse => "on_response",
            Self::OnError => "on_error",
        }
    }
}

/// Request/response mutation (`gts.cf.core.oagw.transform_plugin.v1~*`).
///
/// Executed before and after the proxy call; multiple transforms may be bound.
#[async_trait::async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Registry key: the plugin's full GTS instance id.
    fn id(&self) -> &str;
    /// The GTS instance id this plugin implements.
    fn plugin_type(&self) -> &str;

    /// Phases the plugin participates in; used by the data plane to skip work.
    #[must_use]
    fn phases(&self) -> &'static [TransformPhase] {
        &[
            TransformPhase::OnRequest,
            TransformPhase::OnResponse,
            TransformPhase::OnError,
        ]
    }

    /// Mutate the outbound request.
    ///
    /// # Errors
    /// [`PluginError`] when the plugin fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()>;

    /// Mutate the response before it is returned downstream.
    ///
    /// # Errors
    /// [`PluginError`] when the plugin fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()>;

    /// Mutate the error response.
    ///
    /// # Errors
    /// [`PluginError`] when the plugin fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> PluginResult<()>;
}

/// Which plugin kind a registry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRegistryKind {
    /// `AuthPlugin` registry.
    Auth,
    /// `GuardPlugin` registry.
    Guard,
    /// `TransformPlugin` registry.
    Transform,
}

/// Merge a binding's configuration over a plugin resource's default
/// configuration (the binding wins).
#[must_use]
pub fn merge_config(
    plugin_default: Option<&serde_json::Value>,
    binding: Option<&serde_json::Value>,
) -> serde_json::Value {
    match (plugin_default, binding) {
        (None, None) => serde_json::Value::Null,
        (Some(a), None) => a.clone(),
        (None, Some(b)) => b.clone(),
        (Some(a), Some(b)) => {
            let mut merged = a.clone();
            if let (serde_json::Value::Object(base), serde_json::Value::Object(over)) =
                (&mut merged, b)
            {
                for (k, v) in over {
                    base.insert(k.clone(), v.clone());
                }
            }
            merged
        }
    }
}

/// Resolve a plugin reference to a concrete plugin id.
///
/// Named (built-in) plugins keep their GTS id; custom plugins are keyed by
/// their bare UUID.
#[must_use]
pub fn plugin_registry_key(plugin_ref: &str) -> String {
    plugin_ref.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_config_prefers_the_binding() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let binding = serde_json::json!({"b": 3});
        let merged = merge_config(Some(&base), Some(&binding));
        assert_eq!(merged["a"], 1);
        assert_eq!(merged["b"], 3);
        assert_eq!(merge_config(None, Some(&binding)), binding);
        assert_eq!(merge_config(Some(&base), None), base);
        assert_eq!(merge_config(None, None), serde_json::Value::Null);
    }

    #[test]
    fn guard_decision_statuses() {
        assert_eq!(GuardDecision::allow().rejection_status(), None);
        assert_eq!(
            GuardDecision::reject(502, "REQUIRED_HEADER_MISSING", "missing").rejection_status(),
            Some(502)
        );
    }

    #[test]
    fn plugin_errors_map_to_domain_errors() {
        let err: DomainError = PluginError::unimplemented("gts.x~y").into();
        assert_eq!(err.status(), 503);
        let err: DomainError = PluginError::Internal {
            plugin_id: "p".to_owned(),
            detail: "boom".to_owned(),
        }
        .into();
        assert_eq!(err.status(), 500);
        let err: DomainError = PluginError::SecretUnavailable {
            plugin_id: "p".to_owned(),
            detail: "gone".to_owned(),
        }
        .into();
        assert_eq!(err.status(), 500);
    }
}
