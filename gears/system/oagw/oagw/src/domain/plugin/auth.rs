//! `AuthPlugin` trait and registry (DoD
//! `cpt-cf-oagw-dod-plugin-system-traits`).
//!
//! Auth plugins inject authentication credentials once per request, before
//! guards (execution order in algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain`, step `inst-ps-chain-auth`).
//! The registry is keyed by the canonical GTS identifier — resolution follows
//! algorithm `cpt-cf-oagw-algo-plugin-system-resolve-gts`.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::domain::plugin::RequestContext;

/// An authentication plugin — injects credentials into the outbound request.
///
/// Every implementation advertises its canonical GTS identifier via
/// [`AuthPlugin::plugin_type`] (e.g.
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`) so the registry
/// can resolve bindings by `plugin_ref` (algorithm
/// `cpt-cf-oagw-algo-plugin-system-resolve-gts`).
#[async_trait]
pub trait AuthPlugin: Send + Sync + std::fmt::Debug {
    /// Short plugin name (e.g. `noop`, `apikey`).
    fn id(&self) -> &str;
    /// The canonical GTS plugin identifier of this implementation.
    fn plugin_type(&self) -> &str;
    /// Injects credentials into the request context.
    ///
    /// # Errors
    /// - [`crate::domain::DomainError::AuthenticationFailed`] — the
    ///   credential material could not be used to authenticate (401 on the
    ///   wire).
    /// - [`crate::domain::DomainError::SecretNotFound`] — a referenced
    ///   `cred://` value could not be resolved (500 on the wire).
    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
    ) -> Result<(), crate::domain::DomainError>;
}

/// In-process registry of [`AuthPlugin`] implementations.
///
/// Keyed by GTS identifier (`plugin_ref`); lookups are O(1).  The registry is
/// populated at gear initialization with the built-ins (infra module) plus any
/// externally registered plugins.  Custom (UUID-backed) plugins resolve
/// through the plugin repository, not this registry.
#[derive(Debug, Default)]
pub struct AuthRegistry {
    by_gts_id: DashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an auth plugin under its GTS identifier.
    pub fn register(&self, plugin: Arc<dyn AuthPlugin>) {
        self.by_gts_id
            .insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolves a plugin by its full canonical GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.by_gts_id.get(plugin_ref).map(|r| r.clone())
    }

    /// Whether any plugin is registered under the given GTS identifier.
    #[must_use]
    pub fn contains(&self, plugin_ref: &str) -> bool {
        self.by_gts_id.contains_key(plugin_ref)
    }

    /// Number of registered auth plugins.
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
