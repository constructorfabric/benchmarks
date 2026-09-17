//! `GuardPlugin` trait and registry (DoD
//! `cpt-cf-oagw-dod-plugin-system-traits`).
//!
//! Guard plugins validate requests and enforce policies; they run after the
//! auth phase and can reject.  A successful guard invocation returns
//! [`GuardDecision::Allow`]; a rejection returns a phase-specific
//! [`GuardRejection`] (algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain`, steps
//! `inst-ps-chain-guard-req`/`inst-ps-chain-guard-resp`).

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::domain::plugin::{GuardDecision, RequestContext, ResponseContext};

/// A validation / policy-enforcement plugin.
///
/// `guard_request` runs before the upstream call (can reject the request);
/// `guard_response` runs on the upstream response (can veto a response).
#[async_trait]
pub trait GuardPlugin: Send + Sync + std::fmt::Debug {
    /// Short plugin name (e.g. `required_headers`).
    fn id(&self) -> &str;
    /// The canonical GTS plugin identifier of this implementation.
    fn plugin_type(&self) -> &str;
    /// Validates the outbound request; may reject.
    async fn guard_request(&self, ctx: &RequestContext) -> GuardDecision;
    /// Validates the upstream response; may reject.
    async fn guard_response(&self, ctx: &ResponseContext) -> GuardDecision;
}

/// In-process registry of [`GuardPlugin`] implementations, keyed by GTS
/// identifier.
#[derive(Debug, Default)]
pub struct GuardRegistry {
    by_gts_id: DashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a guard plugin under its GTS identifier.
    pub fn register(&self, plugin: Arc<dyn GuardPlugin>) {
        self.by_gts_id
            .insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolves a plugin by its full canonical GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.by_gts_id.get(plugin_ref).map(|r| r.clone())
    }

    /// Whether any plugin is registered under the given GTS identifier.
    #[must_use]
    pub fn contains(&self, plugin_ref: &str) -> bool {
        self.by_gts_id.contains_key(plugin_ref)
    }

    /// Number of registered guard plugins.
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
