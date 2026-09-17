//! `TransformPlugin` trait and registry (DoD
//! `cpt-cf-oagw-dod-plugin-system-traits`).
//!
//! Transform plugins mutate request, response, and error data around the
//! upstream call (algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain`, steps
//! `inst-ps-chain-transform-req`/`inst-ps-chain-transform-resp`).

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext};

/// A mutation plugin for request/response/error data.
#[async_trait]
pub trait TransformPlugin: Send + Sync + std::fmt::Debug {
    /// Short plugin name (e.g. `request_id`).
    fn id(&self) -> &str;
    /// The canonical GTS plugin identifier of this implementation.
    fn plugin_type(&self) -> &str;
    /// Mutates the outbound request (pre-upstream).
    async fn transform_request(
        &self,
        ctx: &mut RequestContext,
    ) -> Result<(), crate::domain::DomainError>;
    /// Mutates the upstream response (post-success).
    async fn transform_response(
        &self,
        ctx: &mut ResponseContext,
    ) -> Result<(), crate::domain::DomainError>;
    /// Mutates the gateway error envelope (post-error).
    async fn transform_error(
        &self,
        ctx: &mut ErrorContext,
    ) -> Result<(), crate::domain::DomainError>;
}

/// In-process registry of [`TransformPlugin`] implementations, keyed by GTS
/// identifier.
#[derive(Debug, Default)]
pub struct TransformRegistry {
    by_gts_id: DashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a transform plugin under its GTS identifier.
    pub fn register(&self, plugin: Arc<dyn TransformPlugin>) {
        self.by_gts_id
            .insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolves a plugin by its full canonical GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.by_gts_id.get(plugin_ref).map(|r| r.clone())
    }

    /// Whether any plugin is registered under the given GTS identifier.
    #[must_use]
    pub fn contains(&self, plugin_ref: &str) -> bool {
        self.by_gts_id.contains_key(plugin_ref)
    }

    /// Number of registered transform plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_gts_id.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_gts_id.is_empty()
    }
}
