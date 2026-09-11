//! Transport DTOs of the OAGW gear.
//!
//! Feature 1 (Gear Foundation) defines the request-context record the handlers
//! hand to the error mapping layer. No business DTO exists yet: entries 2.2 to
//! 2.6 add them, each going through the same error mapping.

/// Request context available at the transport layer when a domain error is
/// mapped onto a problem document.
///
/// Every field is optional and independently absent: a problem document
/// carries only the OAGW extension fields the request context provides
/// (`cpt-cf-oagw-algo-error-mapping`). No field may hold credential material
/// or a resolved secret value (`cpt-cf-oagw-principle-cred-isolation`): the
/// request path, the upstream identifier and the host are routing facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorContext {
    /// URI reference identifying the occurrence (the request path).
    pub instance: Option<String>,
    /// Identifier of the upstream the request was routed to.
    pub upstream_id: Option<String>,
    /// Upstream host the request was sent to.
    pub host: Option<String>,
    /// Request path, repeated as an extension field so clients keep it even
    /// when an intermediary rewrites `instance`.
    pub path: Option<String>,
    /// Distributed-tracing correlation identifier.
    pub trace_id: Option<String>,
    /// Alias the request addressed (ADR 0007 `alias`, target-host errors).
    pub alias: Option<String>,
    /// Endpoint hosts the caller may name (ADR 0007 `valid_hosts`).
    pub valid_hosts: Option<Vec<String>>,
    /// The rejected `X-OAGW-Target-Host` value (ADR 0007 `invalid_value`).
    pub invalid_value: Option<String>,
}

/// OAGW extension field: `upstream_id`.
pub const EXTENSION_UPSTREAM_ID: &str = "upstream_id";
/// OAGW extension field: `host`.
pub const EXTENSION_HOST: &str = "host";
/// OAGW extension field: `path`.
pub const EXTENSION_PATH: &str = "path";
/// OAGW extension field: `retry_after_seconds`.
pub const EXTENSION_RETRY_AFTER_SECONDS: &str = "retry_after_seconds";
/// OAGW extension field: `trace_id`.
pub const EXTENSION_TRACE_ID: &str = "trace_id";
/// ADR 0007 extension field: the alias the request addressed.
pub const EXTENSION_ALIAS: &str = "alias";
/// ADR 0007 extension field: the endpoint hosts the caller may name.
pub const EXTENSION_VALID_HOSTS: &str = "valid_hosts";
/// ADR 0007 extension field: the rejected routing-header value.
pub const EXTENSION_INVALID_VALUE: &str = "invalid_value";

// The `plugin_id` / `referenced_by` members of the `PluginInUse` conflict are
// owned by the domain error that carries them, so the transport only re-exports
// the names it registers in the problem-document schema.
pub use crate::domain::error::{EXTENSION_PLUGIN_ID, EXTENSION_REFERENCED_BY};

impl ErrorContext {
    /// Context for a request that reached the gear at `path`.
    ///
    /// Fills both `instance` (the RFC 9457 standard field) and the OAGW `path`
    /// extension field from the same request path.
    #[must_use]
    pub fn for_request(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            instance: Some(path.clone()),
            path: Some(path),
            ..Self::default()
        }
    }

    /// Attach an upstream identifier.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attach an upstream host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attach a tracing identifier.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Attach the alias the request addressed.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    /// Attach the endpoint hosts the caller may name.
    #[must_use]
    pub fn with_valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.valid_hosts = Some(hosts);
        self
    }

    /// Attach the rejected routing-header value.
    #[must_use]
    pub fn with_invalid_value(mut self, value: impl Into<String>) -> Self {
        self.invalid_value = Some(value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_context_has_no_extension_field() {
        let context = ErrorContext::default();
        assert!(context.instance.is_none());
        assert!(context.upstream_id.is_none());
        assert!(context.host.is_none());
        assert!(context.path.is_none());
        assert!(context.trace_id.is_none());
        assert!(context.alias.is_none());
        assert!(context.valid_hosts.is_none());
        assert!(context.invalid_value.is_none());
    }

    #[test]
    fn for_request_fills_instance_and_path() {
        let context = ErrorContext::for_request("/oagw/v1/payments");
        assert_eq!(context.instance.as_deref(), Some("/oagw/v1/payments"));
        assert_eq!(context.path.as_deref(), Some("/oagw/v1/payments"));
        assert!(context.upstream_id.is_none());
        assert!(context.host.is_none());
        assert!(context.trace_id.is_none());
    }

    #[test]
    fn builders_attach_only_the_provided_context() {
        let context = ErrorContext::default()
            .with_upstream_id("u-1")
            .with_host("payments.internal:8443")
            .with_trace_id("trace-1");
        assert_eq!(context.upstream_id.as_deref(), Some("u-1"));
        assert_eq!(context.host.as_deref(), Some("payments.internal:8443"));
        assert_eq!(context.trace_id.as_deref(), Some("trace-1"));
        assert!(context.instance.is_none());
        assert!(context.path.is_none());
    }

    #[test]
    fn extension_field_names_match_the_interface_contract() {
        assert_eq!(EXTENSION_UPSTREAM_ID, "upstream_id");
        assert_eq!(EXTENSION_HOST, "host");
        assert_eq!(EXTENSION_PATH, "path");
        assert_eq!(EXTENSION_RETRY_AFTER_SECONDS, "retry_after_seconds");
        assert_eq!(EXTENSION_TRACE_ID, "trace_id");
        assert_eq!(EXTENSION_ALIAS, "alias");
        assert_eq!(EXTENSION_VALID_HOSTS, "valid_hosts");
        assert_eq!(EXTENSION_INVALID_VALUE, "invalid_value");
    }

    #[test]
    fn adr_0007_fields_are_carried_for_the_target_host_errors() {
        let context = ErrorContext::default()
            .with_alias("api.vendor.com")
            .with_valid_hosts(vec!["a.vendor.com".to_owned(), "b.vendor.com".to_owned()])
            .with_invalid_value("a.vendor.com:443");
        assert_eq!(context.alias.as_deref(), Some("api.vendor.com"));
        assert_eq!(
            context.valid_hosts.as_deref(),
            Some(
                &["a.vendor.com".to_owned(), "b.vendor.com".to_owned()][..]
            )
        );
        assert_eq!(context.invalid_value.as_deref(), Some("a.vendor.com:443"));
        // The routing facts stay routing facts: no credential material lands in
        // the record.
        let rendered = format!("{context:?}").to_lowercase();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("bearer"));
    }

    #[test]
    fn context_carries_no_credential_material() {
        // `cpt-cf-oagw-principle-cred-isolation`: the record is a plain
        // routing-context value, and `Debug` renders only routing fields.
        let context = ErrorContext::for_request("/oagw/v1/x").with_host("h");
        let rendered = format!("{context:?}");
        assert!(!rendered.to_lowercase().contains("password"));
        assert!(!rendered.to_lowercase().contains("secret"));
        assert!(!rendered.to_lowercase().contains("bearer"));
    }
}
