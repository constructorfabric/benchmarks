//! Plugin system — traits, contexts, registries.
//!
//! ADR-0002 defines three plugin traits (`AuthPlugin`, `GuardPlugin`,
//! `TransformPlugin`), deterministic execution order
//! (Auth → Guards → Transform(request) → upstream → Transform(response/error))
//! and a registry-based resolution model keyed by GTS identifier.

pub mod builtins;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use toolkit_security::SecurityContext;

/// Errors surfaced by plugin execution.  The data plane maps these onto the
/// documented error taxonomy (401 `AuthenticationFailed` / 503
/// `PluginNotFound` / guard rejections with their own status + error code).
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin rejected request: {detail}")]
    Reject {
        status: u16,
        error_code: String,
        detail: String,
    },
    #[error("plugin configuration error: {0}")]
    Config(String),
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("plugin internal error: {0}")]
    Internal(String),
}

/// Decision returned by guard plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Reject {
        status: u16,
        error_code: String,
        detail: String,
    },
}

/// Mutable request view handed to plugins (before the upstream call).
#[derive(Debug)]
pub struct RequestContext {
    pub config: Value,
    pub method: http::Method,
    pub uri: http::Uri,
    pub headers: http::HeaderMap,
    pub security_context: SecurityContext,
}

impl RequestContext {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Mutable response view handed to plugins (after the upstream call).
///
/// `request_id` is the `X-Request-ID` value that was sent upstream; the
/// `request_id` transform plugin echoes it onto the response.
#[derive(Debug)]
pub struct ResponseContext {
    pub config: Value,
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub request_id: Option<String>,
}

/// Error context for `transform_error` hooks.
#[derive(Debug)]
pub struct ErrorContext {
    pub config: Value,
    /// GTS error instance identifier (problem `type`).
    pub error_type: String,
    pub status: u16,
}

#[async_trait]
pub trait AuthPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

#[async_trait]
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

#[async_trait]
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// Auth plugins keyed by full GTS identifier.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_type).cloned()
    }
}

/// Guard plugins keyed by full GTS identifier.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_type).cloned()
    }
}

/// Transform plugins keyed by full GTS identifier.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_type).cloned()
    }
}
