//! Model-policy gateway: lazy plugin resolution through types-registry and
//! bounded plugin calls (`cpt-cf-mini-chat-dod-gear-foundation-bundled-plugins`).
//!
//! The gateway is built in `init` without resolving anything; the first call
//! resolves the plugin of the gear `vendor` with the lowest priority
//! (`cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`) and caches its id.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, UserLimits,
};
use toolkit::client_hub::ClientHub;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::infra::plugin_registry::{ClientHubLookup, TypesRegistryInstanceSource};
use crate::infra::plugin_resolution::{PluginResolutionError, PluginResolver};

/// Bound of one read call on the model-policy plugin.
pub const POLICY_PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Model-policy gateway failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyGatewayError {
    /// No model-policy plugin of the gear vendor is registered.
    #[error("no model policy plugin is registered")]
    NotRegistered,
    /// The plugin could not be resolved.
    #[error(transparent)]
    Resolution(#[from] PluginResolutionError),
    /// The plugin call failed.
    #[error(transparent)]
    Plugin(#[from] PolicyPluginError),
    /// The plugin call exceeded `POLICY_PLUGIN_CALL_TIMEOUT`.
    #[error("model policy plugin call timed out")]
    Timeout,
}

impl PolicyGatewayError {
    /// The domain error callers return: the cause is kept as a diagnostic for
    /// the log and replaced by a generic `internal` problem on the wire
    /// (`DomainError::PolicyResolution` carries text by contract, which is why
    /// this is a method and not a `From` impl).
    #[must_use]
    pub fn into_domain(self) -> DomainError {
        DomainError::PolicyResolution(self.to_string())
    }
}

/// Gateway to the model-policy plugin.
pub struct ModelPolicyGateway {
    resolver: PluginResolver<dyn MiniChatModelPolicyPluginClientV1>,
    call_timeout: Duration,
}

impl ModelPolicyGateway {
    /// Creates the gateway over the real types-registry and `ClientHub`
    /// adapters; the registry query is bounded by `REGISTRY_QUERY_TIMEOUT`.
    #[must_use]
    pub fn new(hub: &Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        let source = Arc::new(TypesRegistryInstanceSource::new(
            Arc::clone(hub),
            MiniChatModelPolicyPluginSpecV1::gts_type_id().to_string(),
        ));
        let clients = Arc::new(ClientHubLookup::new(Arc::clone(hub)));
        Self::from_resolver(PluginResolver::new(vendor, source, clients))
    }

    /// Creates the gateway over an existing resolver.
    #[must_use]
    pub fn from_resolver(resolver: PluginResolver<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self {
            resolver,
            call_timeout: POLICY_PLUGIN_CALL_TIMEOUT,
        }
    }

    /// Overrides the bound of one plugin call.
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Resolves the plugin client.
    ///
    /// # Errors
    ///
    /// [`PolicyGatewayError::NotRegistered`] when no plugin is registered,
    /// [`PolicyGatewayError::Resolution`] when the registry query fails, times
    /// out or the selected instance has no client.
    pub async fn client(
        &self,
    ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, PolicyGatewayError> {
        self.resolver
            .resolve()
            .await?
            .ok_or(PolicyGatewayError::NotRegistered)
    }

    /// Current policy version of the user.
    ///
    /// # Errors
    ///
    /// Any [`PolicyGatewayError`].
    pub async fn current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyGatewayError> {
        self.call(|client| async move { client.get_current_policy_version(user_id).await })
            .await
    }

    /// Immutable snapshot of `(user_id, policy_version)`.
    ///
    /// # Errors
    ///
    /// Any [`PolicyGatewayError`].
    pub async fn policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyGatewayError> {
        self.call(|client| async move { client.get_policy_snapshot(user_id, policy_version).await })
            .await
    }

    /// Limits of `(user_id, policy_version)`.
    ///
    /// # Errors
    ///
    /// Any [`PolicyGatewayError`].
    pub async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyGatewayError> {
        self.call(|client| async move { client.get_user_limits(user_id, policy_version).await })
            .await
    }

    /// Resolves the client and runs one call bounded by the call timeout.
    async fn call<T, F, Fut>(&self, call: F) -> Result<T, PolicyGatewayError>
    where
        F: FnOnce(Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Fut,
        Fut: Future<Output = Result<T, PolicyPluginError>>,
    {
        let client = self.client().await?;
        tokio::time::timeout(self.call_timeout, call(client))
            .await
            .map_err(|_elapsed| PolicyGatewayError::Timeout)?
            .map_err(PolicyGatewayError::from)
    }
}

#[async_trait]
impl PolicyCatalog for ModelPolicyGateway {
    async fn current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, DomainError> {
        Self::current_policy_version(self, user_id)
            .await
            .map_err(PolicyGatewayError::into_domain)
    }

    async fn policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        Self::policy_snapshot(self, user_id, policy_version)
            .await
            .map_err(PolicyGatewayError::into_domain)
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        Self::user_limits(self, user_id, policy_version)
            .await
            .map_err(PolicyGatewayError::into_domain)
    }
}

#[cfg(test)]
#[path = "policy_gateway_tests.rs"]
mod tests;
