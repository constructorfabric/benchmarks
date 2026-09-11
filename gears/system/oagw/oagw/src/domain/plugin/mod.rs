//! Plugin contracts, exactly as `docs/ADR/0002-plugin-system.md` specifies.
//!
//! Three classes of plugin run against an exchange, in the order
//! **Auth → Guards → Transform(request) → upstream call →
//! Transform(response/error)**. Upstream-level plugins run before route-level
//! plugins. A plugin that rejects short-circuits the chain.

pub mod context;

pub use context::{ErrorContext, GuardDecision, RequestContext, ResponseContext};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;

/// A catalog entry: what an operator can bind to a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    /// Plugin identifier.
    pub id: String,
    /// Class the plugin belongs to.
    pub plugin_type: PluginType,
    /// Implementation version.
    pub version: String,
    /// Operator-facing description.
    pub description: String,
    /// Whether this build ships an implementation.
    pub built_in: bool,
}

/// Read-only view over the plugin catalog, used by the control plane to
/// validate bindings and serve `GET /oagw/v1/plugins`.
pub trait PluginCatalog: Send + Sync {
    /// Every entry, built-ins first.
    fn descriptors(&self) -> Vec<PluginDescriptor>;

    /// A single entry by identifier.
    fn descriptor(&self, id: &str) -> Option<PluginDescriptor>;

    /// Whether `id` has an implementation in this build, and not merely a
    /// catalogue entry: a binding can only be honoured for one that does.
    fn is_implemented(&self, id: &str) -> bool {
        self.descriptor(id)
            .is_some_and(|descriptor| descriptor.built_in)
    }
}

/// Authenticates the caller to the upstream, typically by injecting a
/// credential resolved from the credential store.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identifier in the catalog.
    fn id(&self) -> &'static str;

    /// Class name (`auth`).
    fn plugin_type(&self) -> &'static str;

    /// Inject whatever the upstream requires into `ctx`.
    ///
    /// # Errors
    /// Returns the error that should be rendered to the caller, e.g.
    /// [`ErrorKind::SecretNotFound`] when a referenced secret does not exist.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;
}

/// Validates an exchange before it is forwarded and after a response returns.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identifier in the catalog.
    fn id(&self) -> &'static str;

    /// Class name (`guard`).
    fn plugin_type(&self) -> &'static str;

    /// Inspect the request.
    ///
    /// # Errors
    /// Returns the rejection error when the request must not proceed.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError>;

    /// Inspect the response.
    ///
    /// # Errors
    /// Returns the rejection error when the response must be replaced.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, DomainError>;
}

/// Rewrites the exchange in flight.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identifier in the catalog.
    fn id(&self) -> &'static str;

    /// Class name (`transform`).
    fn plugin_type(&self) -> &'static str;

    /// Mutate the request before it is forwarded.
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;

    /// Mutate the response before it is returned to the caller.
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError>;

    /// Mutate a gateway error before it is rendered.
    ///
    /// # Errors
    /// Returns an error when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError>;
}

/// Plugin class, matching the catalog's `type` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    /// Authenticates the caller to the upstream.
    #[default]
    Auth,
    /// Validates requests and responses.
    Guard,
    /// Rewrites requests, responses and errors.
    Transform,
}

impl PluginType {
    /// The string used in the catalog and in the REST surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

#[cfg(test)]
mod plugin_context_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn request_context_records_injections() {
        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/v1/x".to_owned(),
            query: "a=1".to_owned(),
            headers: http::HeaderMap::new(),
            body_present: false,
            security_context: toolkit_security::SecurityContext::anonymous(),
            tenant_scope: vec![uuid::Uuid::nil()],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        };
        ctx.set_header("X-API-Key", "value");
        assert_eq!(ctx.headers.get("x-api-key").unwrap(), "value");
        assert_eq!(ctx.injected_headers, vec!["x-api-key".to_owned()]);
        ctx.set_attribute("route", "r1");
        assert_eq!(ctx.attributes.get("route").map(String::as_str), Some("r1"));
    }

    #[test]
    fn guard_decision_carries_its_rejection() {
        assert!(GuardDecision::Allow.is_allowed());
        let reject = GuardDecision::Reject(DomainError::validation("nope"));
        assert!(!reject.is_allowed());
        assert_eq!(
            reject
                .rejection()
                .map(crate::domain::error::DomainError::detail),
            Some("nope")
        );
    }

    #[test]
    fn plugin_types_render_catalog_strings() {
        assert_eq!(PluginType::Auth.as_str(), "auth");
        assert_eq!(PluginType::Guard.as_str(), "guard");
        assert_eq!(PluginType::Transform.as_str(), "transform");
    }
}
