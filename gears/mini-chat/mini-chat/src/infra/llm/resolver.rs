//! Provider resolver: maps catalog `provider_id` (+ tenant overrides) to the
//! adapter kind and the OAGW upstream alias.

use std::collections::HashMap;

use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind};

/// A resolved provider endpoint for one tenant.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    /// The `providers.<id>` key.
    pub provider_id: String,
    /// Adapter kind.
    pub kind: ProviderKind,
    /// OAGW upstream alias.
    pub alias: String,
    /// Chat endpoint path (`{model}` placeholder; may carry a query string).
    pub api_path: String,
}

/// A resolved storage (file/vector-store) provider.
#[derive(Debug, Clone)]
pub struct ResolvedStorage {
    /// The provider entry serving storage.
    pub provider_id: String,
    /// Storage implementation.
    pub storage_kind: StorageKind,
    /// OAGW upstream alias.
    pub alias: String,
    /// Storage routing label persisted on attachments / vector store rows.
    pub storage_backend: String,
    /// Azure `api-version` (required for `storage_kind = azure`).
    pub api_version: Option<String>,
}

/// The provider registry built from configuration at gear init.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderEntry>,
}

impl ProviderRegistry {
    /// Build from validated, normalized config.
    #[must_use]
    pub fn new(config: &MiniChatConfig) -> Self {
        Self {
            providers: config.providers.clone(),
        }
    }

    /// Whether any configured entry uses the Anthropic Messages adapter.
    #[must_use]
    pub fn has_anthropic(&self) -> bool {
        self.providers
            .values()
            .any(|p| p.kind == Some(ProviderKind::AnthropicMessages))
    }

    /// Get a provider entry by id.
    #[must_use]
    pub fn entry(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    /// Resolve a provider for a tenant (applies tenant overrides).
    #[must_use]
    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant_id: &str) -> Option<ResolvedProvider> {
        let entry = self.providers.get(provider_id)?;
        let kind = entry.kind?;
        let mut alias = entry
            .upstream_alias
            .clone()
            .unwrap_or_else(|| entry.host.clone());
        if let Some(override_) = entry.tenant_overrides.get(tenant_id) {
            alias = override_
                .upstream_alias
                .clone()
                .or_else(|| override_.host.clone())
                .unwrap_or(alias);
        }
        Some(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind,
            alias,
            api_path: entry.api_path.clone(),
        })
    }

    /// Resolve the storage provider for a chat-serving provider (its
    /// `rag_provider` when set, otherwise the provider itself).
    #[must_use]
    #[must_use]
    pub fn resolve_storage(&self, provider_id: &str, tenant_id: &str) -> Option<ResolvedStorage> {
        let entry = self.providers.get(provider_id)?;
        let storage_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        let storage_entry = self.providers.get(storage_id)?;
        let storage_kind = storage_entry.storage_kind?;
        let mut alias = storage_entry
            .upstream_alias
            .clone()
            .unwrap_or_else(|| storage_entry.host.clone());
        if let Some(override_) = storage_entry.tenant_overrides.get(tenant_id) {
            alias = override_
                .upstream_alias
                .clone()
                .or_else(|| override_.host.clone())
                .unwrap_or(alias);
        }
        Some(ResolvedStorage {
            provider_id: storage_id.to_owned(),
            storage_kind,
            alias,
            storage_backend: storage_entry
                .storage_backend
                .clone()
                .unwrap_or_else(|| storage_id.to_owned()),
            api_version: storage_entry.api_version.clone(),
        })
    }

    /// The Anthropic provider entry for a chat-serving provider, when its
    /// kind is `anthropic_messages` (secondary image copies).
    #[must_use]
    pub fn anthropic_files_target(&self, provider_id: &str, tenant_id: &str) -> Option<String> {
        let entry = self.providers.get(provider_id)?;
        if entry.kind != Some(ProviderKind::AnthropicMessages) {
            return None;
        }
        Some(self.resolve(provider_id, tenant_id)?.alias)
    }
}
