//! Provider resolution: catalog `provider_id` (+ tenant) → provider entry and
//! the OAGW alias to proxy through (D `llm_provider`, ADR-0005).

use std::collections::{BTreeMap, HashMap};

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::DomainError;
use uuid::Uuid;

/// Where and how to send a chat / summary request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// OAGW upstream alias (tenant override applied).
    pub alias: String,
    /// Chat endpoint path, possibly with a `{model}` placeholder.
    pub api_path: String,
}

impl ProviderTarget {
    /// Proxy URI `/{alias}{api_path}` with `{model}` replaced.
    #[must_use]
    pub fn chat_uri(&self, model: &str) -> String {
        format!("/{}{}", self.alias, self.api_path.replace("{model}", model))
    }
}

/// Where file / vector-store operations go (the `rag_provider` entry if set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub provider_id: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend`.
    pub storage_backend: String,
}

/// Resolver over the validated provider entries (aliases already filled by
/// `MiniChatConfig::validate`).
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    providers: BTreeMap<String, ProviderEntry>,
    /// `(provider_id, tenant)` → override alias (keys parsed as UUIDs).
    override_aliases: HashMap<(String, Uuid), String>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: &BTreeMap<String, ProviderEntry>) -> Self {
        let override_aliases = providers
            .iter()
            .flat_map(|(id, p)| {
                p.tenant_overrides.iter().filter_map(move |(tenant, o)| {
                    let tenant = Uuid::parse_str(tenant).ok()?;
                    let alias = o.upstream_alias.clone()?;
                    Some(((id.clone(), tenant), alias))
                })
            })
            .collect();
        Self {
            providers: providers.clone(),
            override_aliases,
        }
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderEntry, DomainError> {
        self.providers.get(provider_id).ok_or_else(|| {
            DomainError::ProviderResolution(format!("unknown provider {provider_id:?}"))
        })
    }

    fn alias(&self, provider_id: &str, entry: &ProviderEntry, tenant_id: Uuid) -> String {
        self.override_aliases
            .get(&(provider_id.to_owned(), tenant_id))
            .or(entry.upstream_alias.as_ref())
            .cloned()
            .unwrap_or_else(|| entry.host.clone())
    }

    /// Chat target of `provider_id` for `tenant_id`.
    ///
    /// # Errors
    /// [`DomainError::ProviderResolution`] for an unknown provider.
    pub fn resolve(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ProviderTarget, DomainError> {
        let entry = self.entry(provider_id)?;
        Ok(ProviderTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: self.alias(provider_id, entry, tenant_id),
            api_path: entry.api_path.clone(),
        })
    }

    /// Storage target of `provider_id` for `tenant_id` (follows `rag_provider`).
    ///
    /// # Errors
    /// [`DomainError::ProviderResolution`] for an unknown provider.
    pub fn resolve_storage(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<StorageTarget, DomainError> {
        let entry = self.entry(provider_id)?;
        let storage_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        let storage = self.entry(storage_id)?;
        Ok(self.storage_target(storage_id, storage, tenant_id))
    }

    /// Storage target of the entry whose storage backend label (the value
    /// stored in `attachments.storage_backend` / `chat_vector_stores.provider`)
    /// is `storage_backend`, for `tenant_id` (first entry in id order).
    ///
    /// # Errors
    /// [`DomainError::ProviderResolution`] when no entry has that label.
    pub fn resolve_storage_backend(
        &self,
        storage_backend: &str,
        tenant_id: Uuid,
    ) -> Result<StorageTarget, DomainError> {
        let (id, entry) = self
            .providers
            .iter()
            .find(|(id, e)| e.effective_storage_backend(id) == storage_backend)
            .ok_or_else(|| {
                DomainError::ProviderResolution(format!(
                    "unknown storage backend {storage_backend:?}"
                ))
            })?;
        Ok(self.storage_target(id, entry, tenant_id))
    }

    /// Kind and Azure storage target (`storage_kind = azure`, its own alias
    /// for `tenant_id` and `api_version`) of the knowledge-search entry
    /// `provider_id`.
    ///
    /// # Errors
    /// [`DomainError::ProviderResolution`] for an unknown provider.
    pub fn knowledge_target(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<(ProviderKind, StorageTarget), DomainError> {
        let entry = self.entry(provider_id)?;
        let mut target = self.storage_target(provider_id, entry, tenant_id);
        target.storage_kind = StorageKind::Azure;
        Ok((entry.kind, target))
    }

    /// Alias for `tenant_id` of the entry `provider_id` when that entry is
    /// of `kind` (`None` for an unknown entry or another kind).
    #[must_use]
    pub fn alias_if_kind(
        &self,
        provider_id: &str,
        kind: ProviderKind,
        tenant_id: Uuid,
    ) -> Option<String> {
        let entry = self.providers.get(provider_id).filter(|e| e.kind == kind)?;
        Some(self.alias(provider_id, entry, tenant_id))
    }

    fn storage_target(&self, id: &str, entry: &ProviderEntry, tenant_id: Uuid) -> StorageTarget {
        StorageTarget {
            provider_id: id.to_owned(),
            storage_kind: entry.storage_kind,
            alias: self.alias(id, entry, tenant_id),
            api_version: entry.api_version.clone(),
            storage_backend: entry.effective_storage_backend(id).to_owned(),
        }
    }
}

#[cfg(test)]
#[path = "provider_resolver_tests.rs"]
mod tests;
