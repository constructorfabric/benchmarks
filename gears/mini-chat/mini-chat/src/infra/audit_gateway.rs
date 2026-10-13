//! Audit gateway: lazy plugin resolution through types-registry and bounded
//! `emit` calls (`cpt-cf-mini-chat-dod-gear-foundation-bundled-plugins`).

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::ClientHub;

use crate::infra::plugin_registry::{ClientHubLookup, TypesRegistryInstanceSource};
use crate::infra::plugin_resolution::{PluginResolutionError, PluginResolver};

/// Bound of one `emit` call on the audit plugin.
pub const AUDIT_EMIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Audit gateway failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditGatewayError {
    /// No audit plugin of the gear vendor is registered.
    #[error("no audit plugin is registered")]
    NotRegistered,
    /// The plugin could not be resolved.
    #[error(transparent)]
    Resolution(#[from] PluginResolutionError),
    /// The plugin call failed.
    #[error(transparent)]
    Plugin(#[from] AuditPluginError),
}

/// Gateway to the audit plugin.
pub struct AuditGateway {
    resolver: PluginResolver<dyn MiniChatAuditPluginClientV1>,
    emit_timeout: Duration,
}

impl AuditGateway {
    /// Creates the gateway over the real types-registry and `ClientHub`
    /// adapters; the registry query is bounded by `REGISTRY_QUERY_TIMEOUT`.
    #[must_use]
    pub fn new(hub: &Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        let source = Arc::new(TypesRegistryInstanceSource::new(
            Arc::clone(hub),
            MiniChatAuditPluginSpecV1::gts_type_id().to_string(),
        ));
        let clients = Arc::new(ClientHubLookup::new(Arc::clone(hub)));
        Self::from_resolver(PluginResolver::new(vendor, source, clients))
    }

    /// Creates the gateway over an existing resolver.
    #[must_use]
    pub fn from_resolver(resolver: PluginResolver<dyn MiniChatAuditPluginClientV1>) -> Self {
        Self {
            resolver,
            emit_timeout: AUDIT_EMIT_TIMEOUT,
        }
    }

    /// Overrides the bound of one `emit` call.
    #[must_use]
    pub fn with_emit_timeout(mut self, timeout: Duration) -> Self {
        self.emit_timeout = timeout;
        self
    }

    /// Resolves the plugin client.
    ///
    /// # Errors
    ///
    /// [`AuditGatewayError::NotRegistered`] when no plugin is registered,
    /// [`AuditGatewayError::Resolution`] when the registry query fails, times out
    /// or the selected instance has no client.
    pub async fn client(&self) -> Result<Arc<dyn MiniChatAuditPluginClientV1>, AuditGatewayError> {
        self.resolver
            .resolve()
            .await?
            .ok_or(AuditGatewayError::NotRegistered)
    }

    /// Emits one audit event, bounded by the emit timeout.
    ///
    /// # Errors
    ///
    /// Any [`AuditGatewayError`]; an elapsed bound is reported as
    /// [`AuditPluginError::PluginTimeout`].
    pub async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditGatewayError> {
        let client = self.client().await?;
        tokio::time::timeout(self.emit_timeout, client.emit(event))
            .await
            .map_err(|_elapsed| AuditPluginError::PluginTimeout)?
            .map_err(AuditGatewayError::from)
    }
}

#[cfg(test)]
#[path = "audit_gateway_tests.rs"]
mod tests;
