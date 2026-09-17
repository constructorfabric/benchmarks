//! Plugin System — the three plugin trait families and their registries
//! (feature `cpt-cf-oagw-feature-plugin-system`, component
//! `cpt-cf-oagw-component-plugin-system`).
//!
//! Implements DoD `cpt-cf-oagw-dod-plugin-system-traits` (the `AuthPlugin`,
//! `GuardPlugin`, `TransformPlugin` traits with `id()` / `plugin_type()`
//! accessors and phase methods), DoD
//! `cpt-cf-oagw-dod-plugin-system-identification` (the `plugin_ref` /
//! `plugin_uuid` identification model and binding-conflict detection),
//! algorithm `cpt-cf-oagw-algo-plugin-system-resolve-gts` (GTS-identifier
//! resolution against the three registries), and algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain` (deterministic execution
//! order: Auth → Guards → Transform(request) → upstream →
//! Transform(response/error), upstream plugins before route plugins).
//!
//! The plugin *interfaces* are domain contracts (this module); the built-in
//! implementations and the registry construction live in
//! [`crate::infra::plugin`].

pub mod auth;
pub mod chain;
pub mod guard;
pub mod ids;
pub mod registries;
pub mod transform;

use std::collections::HashSet;

pub use auth::{AuthPlugin, AuthRegistry};
pub use chain::{ChainOutcome, PluginChain};
pub use guard::{GuardPlugin, GuardRegistry};
pub use ids::{CATALOG_ONLY, GtsPluginRef};
pub use registries::{PluginRegistries, ResolvedPlugin};
pub use transform::{TransformPlugin, TransformRegistry};

/// HTTP header collection used by plugin contexts.
///
/// A case-insensitive, ordered multimap so phase handling (header presence
/// checks, credential injection, propagation) behaves like real HTTP
/// semantics without pulling an HTTP vocabulary into the domain layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    /// `(lowercased_name, value)` pairs; original casing is not preserved.
    entries: Vec<(String, String)>,
}

impl Headers {
    /// Creates an empty header collection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first value for a (case-insensitive) header name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|(n, _)| *n == lower)
            .map(|(_, v)| v.as_str())
    }

    /// All values for a (case-insensitive) header name.
    pub fn all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        let lower = name.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(move |(n, _)| *n == lower)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the named header is present (case-insensitive).
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.entries.iter().any(|(n, _)| *n == lower)
    }

    /// Inserts a header, replacing any existing values for the same
    /// (case-insensitive) name.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let lower = name.into().to_ascii_lowercase();
        self.entries.retain(|(n, _)| *n != lower);
        self.entries.push((lower, value.into()));
    }

    /// Appends a value, keeping existing values for the same name.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries
            .push((name.into().to_ascii_lowercase(), value.into()));
    }

    /// Removes every value for the named header.
    pub fn remove(&mut self, name: &str) {
        let lower = name.to_ascii_lowercase();
        self.entries.retain(|(n, _)| *n != lower);
    }

    /// The (lowercased) header names present.
    #[must_use]
    pub fn names(&self) -> HashSet<&str> {
        self.entries.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Iterates over all `(name, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Number of header entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the collection is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FromIterator<(String, String)> for Headers {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        let entries = iter
            .into_iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v))
            .collect();
        Self { entries }
    }
}

/// Per-request context handed to plugin phase methods.
///
/// Carries the inbound request surface, the plugin binding's `config`, and
/// the authenticated security identity so plugins that resolve `cred://`
/// material (apikey, OAuth2) can honour CredStore tenant access checks
/// (principle `cpt-cf-oagw-principle-cred-isolation`).
///
/// `Clone` lets the chain hand each guard/transform binding its own `config`
/// without mutating the shared request surface (guards take the context by
/// shared reference).
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// HTTP method of the inbound request.
    pub method: String,
    /// Request path (without query string).
    pub path: String,
    /// Query parameters as `(name, value)` pairs.
    pub query: Vec<(String, String)>,
    pub headers: Headers,
    /// The effective plugin configuration for this binding.
    pub config: serde_json::Value,
    /// The authenticated identity for CredStore access checks; `None` when
    /// no authenticated subject exists.
    pub security: Option<toolkit_security::SecurityContext>,
}

/// Per-response context handed to guard/transform *response* phases.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// HTTP status of the response.
    pub status: u16,
    pub headers: Headers,
    /// The effective plugin configuration for this binding.
    pub config: serde_json::Value,
}

/// Per-error context handed to the transform *error* phase.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// HTTP status of the error response.
    pub status: u16,
    pub headers: Headers,
    /// The gateway error detail (never secret material).
    pub detail: String,
    /// The effective plugin configuration for this binding.
    pub config: serde_json::Value,
}

/// A guard rejection — the phase, wire status, machine-readable code, and
/// human detail (algorithm
/// `cpt-cf-oagw-algo-plugin-system-required-headers`, step
/// `inst-ps-rh-reject`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRejection {
    /// The phase in which the guard rejected.
    pub phase: GuardPhase,
    /// Wire status (request phase 400, response phase 502 for
    /// `required_headers`).
    pub status: u16,
    /// Machine-readable rejection code (e.g. `REQUIRED_HEADER_MISSING`).
    pub code: String,
    /// Human-readable detail.
    pub detail: String,
}

impl GuardRejection {
    /// Maps a guard rejection to the closest DESIGN error-catalog instance.
    ///
    /// The DESIGN error table has no dedicated guard-rejection instance; the
    /// `required_headers` guard's 400/`REQUIRED_HEADER_MISSING` maps to
    /// `validation.error` (400) and its 502 response-phase rejection to
    /// `protocol.error` (502) — the closest instances in the same status
    /// class.  The Data-Plane Proxy feature (p5) carries the phase- and
    /// code-aware RFC 9457 envelope on the wire; this mapping is the
    /// canonical/error-framework bridge.
    #[must_use]
    pub fn to_domain_error(&self) -> crate::domain::DomainError {
        let detail = format!("{}: {}", self.code, self.detail);
        if self.status < 500 {
            crate::domain::DomainError::validation(None, detail)
        } else {
            crate::domain::DomainError::ProtocolError {
                detail,
                cause: None,
            }
        }
    }
}

/// The phase in which a guard rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardPhase {
    Request,
    Response,
}

/// A guard's decision for one phase invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Reject(GuardRejection),
}

impl GuardDecision {
    #[must_use]
    pub fn allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: vec![("top".to_owned(), "5".to_owned())],
            config: serde_json::json!({}),
            headers: Headers::new(),
            security: None,
        }
    }

    #[test]
    fn headers_are_case_insensitive_and_multivalued() {
        let mut h = Headers::new();
        h.append("X-Request-ID", "req-1");
        h.append("x-request-id", "req-2");
        h.append("Authorization", "Bearer abc");
        assert_eq!(h.get("X-REQUEST-ID"), Some("req-1"));
        assert_eq!(h.all("x-request-id").count(), 2);
        assert!(h.contains("AUTHORIZATION"));
        h.insert("AUTHORIZATION", "Bearer def");
        assert_eq!(h.all("authorization").count(), 1);
        assert_eq!(h.get("authorization"), Some("Bearer def"));
        h.remove("X-Request-ID");
        assert!(!h.contains("x-request-id"));
    }

    #[test]
    fn headers_iter_and_from_iter() {
        let h: Headers = vec![("X-A".to_owned(), "1".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![("x-a", "1")]);
        assert_eq!(h.len(), 1);
        assert!(!h.is_empty());
    }

    #[test]
    fn guard_rejection_maps_closest_catalog_instance() {
        let req = GuardRejection {
            phase: GuardPhase::Request,
            status: 400,
            code: "REQUIRED_HEADER_MISSING".to_owned(),
            detail: "X-Tenant".to_owned(),
        };
        let err = req.to_domain_error();
        assert_eq!(err.status(), 400);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );

        let resp = GuardRejection {
            phase: GuardPhase::Response,
            status: 502,
            code: "REQUIRED_HEADER_MISSING".to_owned(),
            detail: "X-Upstream".to_owned(),
        };
        let err = resp.to_domain_error();
        assert_eq!(err.status(), 502);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
        );
    }

    #[test]
    fn request_context_builds_with_security() {
        let c = ctx();
        assert_eq!(c.method, "GET");
        assert!(c.security.is_none());
    }
}
