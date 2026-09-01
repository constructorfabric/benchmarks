//! Plugin contracts (ADR 0002 "Plugin System — Three Plugin Types").
//!
//! The three traits below are the data-plane extension points: built-in and
//! external gears implement them identically, and the proxy executes them in
//! the fixed order Auth → Guard → Transform(request) → upstream →
//! Transform(response/error). The control plane only needs the *catalogue*
//! (which identifiers exist and which of them are bindable) plus the registry
//! seam, so no plugin implementation lives here.
//!
//! Review evidence (privilege boundary — plugin binding surface):
//! * Guardrail: DESIGN §3.1 "Plugin Schemas" + PRD §5.3 "Built-in Plugins".
//! * Rationale: catalog-only identifiers (`basic`, `bearer`, `timeout`,
//!   `cors`, `logging`, `metrics`) have no backing implementation, so binding
//!   them would silently produce a plugin chain that cannot be resolved at
//!   proxy time. [`builtin_plugin_catalog`] marks them explicitly and
//!   [`builtin_plugin_is_bindable`] is consulted by the control-plane
//!   validation.
//! * Validation performed: `plugin_catalog_tests` pins the full catalogue and
//!   asserts that exactly the six documented identifiers are non-bindable.

pub mod builtins;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;

pub use crate::domain::models::{PluginKind, PluginPhase};
pub use builtins::{builtin_plugin_catalog, builtin_plugin_is_bindable, BuiltinPlugin};

/// Verdict returned by a guard plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardDecision {
    /// Continue processing.
    Allow,
    /// Reject the request with a domain error (400/413/429/…).
    Reject(DomainError),
}

impl GuardDecision {
    /// Whether the guard allowed the request.
    #[must_use]
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Data-plane request context handed to plugins.
///
/// Populated by the proxy engine (S5+); the control plane never constructs
/// one. Keeping the type here means the plugin traits stay in the domain
/// layer, as required by the DDD-Light boundary.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// Calling tenant, used by credential caches for isolation (ADR 0008).
    pub tenant_id: Uuid,
    /// Authenticated subject, used by credential caches for isolation.
    pub subject_id: Uuid,
    /// Upstream being proxied, when already resolved.
    pub upstream_id: Option<Uuid>,
    /// Matched route, when already resolved.
    pub route_id: Option<Uuid>,
    /// Request target path.
    pub path: String,
    /// Request method.
    pub method: String,
    /// Inbound headers (name, value), in arrival order.
    pub headers: Vec<(String, String)>,
    /// Headers to be sent upstream, in insertion order.
    pub outbound_headers: Vec<(String, String)>,
    /// Plugin configuration block.
    pub config: serde_json::Value,
}

impl RequestContext {
    /// Header value lookup (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Sets (replacing) an outbound header.
    pub fn set_outbound_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        if let Some(slot) = self
            .outbound_headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        {
            slot.1 = value.into();
        } else {
            self.outbound_headers.push((name, value.into()));
        }
    }
}

/// Data-plane response context handed to transform plugins.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// Status returned by the upstream.
    pub status: u16,
    /// Response headers, in arrival order.
    pub headers: Vec<(String, String)>,
    /// Whether the body is streamed.
    pub streaming: bool,
    /// Plugin configuration block.
    pub config: serde_json::Value,
}

/// Data-plane error context handed to transform plugins.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// Domain error under construction.
    pub error: Option<DomainError>,
}

/// Credential injection plugin (executed once per request, before guards).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Registry key (short name).
    fn id(&self) -> &'static str;
    /// Full GTS identifier.
    fn plugin_type(&self) -> &'static str;
    /// Injects credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when authentication cannot be performed.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;
}

/// Validation plugin (can reject requests).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Registry key (short name).
    fn id(&self) -> &'static str;
    /// Full GTS identifier.
    fn plugin_type(&self) -> &'static str;
    /// Validates the inbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the request must be rejected.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError>;
    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the response must be rejected.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, DomainError>;
}

/// Request/response/error mutation plugin.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Registry key (short name).
    fn id(&self) -> &'static str;
    /// Full GTS identifier.
    fn plugin_type(&self) -> &'static str;
    /// Declared phases; used to skip hooks the plugin does not implement.
    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::OnRequest, PluginPhase::OnResponse]
    }
    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the transformation fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError>;
    /// Mutates the response returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the transformation fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError>;
    /// Mutates the error returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the transformation fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError>;
}

/// In-process plugin registry (ADR 0002 "Plugin Loading").
#[derive(Default)]
pub struct PluginRegistry {
    auth: HashMap<String, Arc<dyn AuthPlugin>>,
    guard: HashMap<String, Arc<dyn GuardPlugin>>,
    transform: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl PluginRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an auth plugin under its [`AuthPlugin::id`].
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a guard plugin under its [`GuardPlugin::id`].
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guard.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a transform plugin under its [`TransformPlugin::id`].
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transform.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves an auth plugin by registry key or full GTS identifier.
    #[must_use]
    pub fn auth(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        if let Some(found) = self.auth.get(reference) {
            return Some(Arc::clone(found));
        }
        self.auth
            .values()
            .find(|plugin| plugin.plugin_type() == reference)
            .cloned()
    }

    /// Resolves a guard plugin by registry key or full GTS identifier.
    #[must_use]
    pub fn guard(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        if let Some(found) = self.guard.get(reference) {
            return Some(Arc::clone(found));
        }
        self.guard
            .values()
            .find(|plugin| plugin.plugin_type() == reference)
            .cloned()
    }

    /// Resolves a transform plugin by registry key or full GTS identifier.
    #[must_use]
    pub fn transform(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        if let Some(found) = self.transform.get(reference) {
            return Some(Arc::clone(found));
        }
        self.transform
            .values()
            .find(|plugin| plugin.plugin_type() == reference)
            .cloned()
    }

    /// Whether any plugin family resolves `reference`.
    #[must_use]
    pub fn resolves(&self, reference: &str) -> bool {
        self.auth(reference).is_some()
            || self.guard(reference).is_some()
            || self.transform(reference).is_some()
    }
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth.len())
            .field("guard", &self.guard.len())
            .field("transform", &self.transform.len())
            .finish()
    }
}
