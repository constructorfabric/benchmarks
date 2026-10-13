//! Provider resolver (`cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`).
//!
//! Maps a catalog `provider_id` plus the request tenant to the adapter kind,
//! the OAGW alias, the chat `api_path` and the storage provider id.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-provider-registry:p1

use uuid::Uuid;

use crate::config::{ProviderKind, ProviderRegistry, ResolvedProvider};
use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::{StorageTarget, UploadTargets};
use crate::domain::service::attachments::ports::StorageDirectory;
use crate::domain::service::stream::ports::{ProviderDirectory, ProviderTarget};

/// Result of a provider resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResolution {
    /// Adapter kind of the entry.
    pub kind: ProviderKind,
    /// OAGW alias (the override alias for an overridden tenant, else the entry alias).
    pub alias: String,
    /// Chat endpoint path (`{model}` placeholder kept).
    pub api_path: String,
    /// Provider id used for file and vector-store operations: `rag_provider`
    /// when configured, otherwise the provider itself.
    pub storage_provider_id: String,
}

/// Resolves `provider_id` plus tenant against the validated provider entries.
///
/// The resolver keeps the configured aliases; the alias OAGW returns at
/// provisioning time is never written here.
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    providers: Vec<ResolvedProvider>,
}

impl ProviderResolver {
    /// Builds the resolver from the validated registry.
    #[must_use]
    pub fn new(registry: &ProviderRegistry) -> Self {
        Self {
            providers: registry.providers.clone(),
        }
    }

    /// Resolves the provider for `tenant_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProviderResolution`] (mapped to 500 `internal`) naming
    /// `provider_id` when the entry is not configured.
    pub fn resolve(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ProviderResolution, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-lookup-provider
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| {
                DomainError::ProviderResolution(format!(
                    "provider `{provider_id}` is not configured"
                ))
            })?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-lookup-provider
        let tenant = tenant_id.to_string();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-if-tenant-override
        let alias = if let Some(over) = provider
            .tenant_overrides
            .iter()
            .find(|over| over.tenant_id == tenant)
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-use-override-alias
            &over.upstream_alias
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-use-override-alias
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-else-entry-alias
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-use-entry-alias
            &provider.upstream_alias
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-use-entry-alias
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-else-entry-alias
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-if-tenant-override
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-storage-provider
        let storage_provider_id = provider
            .rag_provider
            .clone()
            .unwrap_or_else(|| provider.id.clone());
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-storage-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-return-resolution
        Ok(ProviderResolution {
            kind: provider.kind,
            alias: alias.clone(),
            api_path: provider.api_path.clone(),
            storage_provider_id,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-provider:p1:inst-return-resolution
    }
}

impl ProviderDirectory for ProviderResolver {
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError> {
        let resolution = ProviderResolver::resolve(self, provider_id, tenant_id)?;
        let api_version = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .and_then(|provider| provider.api_version.clone());
        Ok(ProviderTarget {
            kind: resolution.kind,
            alias: resolution.alias,
            api_path: resolution.api_path,
            api_version,
        })
    }
}

/// The storage target of `provider`'s entry for `tenant_id`.
fn storage_target_of(provider: &ResolvedProvider, tenant_id: Uuid) -> StorageTarget {
    let tenant = tenant_id.to_string();
    let alias = provider
        .tenant_overrides
        .iter()
        .find(|over| over.tenant_id == tenant)
        .map_or_else(
            || provider.upstream_alias.clone(),
            |over| over.upstream_alias.clone(),
        );
    StorageTarget {
        provider_id: provider.id.clone(),
        kind: provider.storage_kind,
        backend: provider.storage_backend.clone(),
        alias,
        api_version: provider.api_version.clone(),
    }
}

/// The provider entries as storage targets of the attachments feature
/// (`cpt-cf-mini-chat-algo-attachments-resolve-upload-context`, steps 3 to 5, and
/// the backend lookup of `cpt-cf-mini-chat-algo-attachments-handle-cleanup`).
impl StorageDirectory for ProviderResolver {
    fn upload_targets(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<UploadTargets, DomainError> {
        let resolution = self.resolve(provider_id, tenant_id)?;
        let storage = self
            .providers
            .iter()
            .find(|provider| provider.id == resolution.storage_provider_id)
            .map(|provider| storage_target_of(provider, tenant_id))
            .ok_or_else(|| {
                DomainError::ProviderResolution(format!(
                    "storage provider `{}` is not configured",
                    resolution.storage_provider_id
                ))
            })?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-secondary-target
        let secondary_alias =
            (resolution.kind == ProviderKind::AnthropicMessages).then(|| resolution.alias.clone());
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-resolve-upload-context:p1:inst-secondary-target
        Ok(UploadTargets {
            storage,
            secondary_alias,
        })
    }

    fn storage_for_backend(&self, backend: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        self.providers
            .iter()
            .find(|provider| provider.storage_backend == backend)
            .map(|provider| storage_target_of(provider, tenant_id))
    }
}

#[cfg(test)]
#[path = "provider_registry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "provider_registry_storage_tests.rs"]
mod storage_tests;
