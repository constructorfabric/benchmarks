//! Provider resolver: catalog `provider_id` + tenant -> provider entry and OAGW alias.
//!
//! Built at gear init from the alias-filled provider entries
//! ([`MiniChatConfig::fill_upstream_aliases`]); it routes by the configured alias,
//! never by the alias OAGW returns at provisioning (DESIGN §3.2).

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind};
use crate::domain::error::DomainError;

use super::knowledge::KnowledgeTarget;
use super::types::{ResolvedProvider, ResolvedStorage};

pub struct ProviderResolver {
    providers: HashMap<String, ProviderEntry>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(cfg: &MiniChatConfig) -> Self {
        Self {
            providers: cfg.providers.clone(),
        }
    }

    /// Resolve a catalog `provider_id` for `tenant_id`.
    ///
    /// # Errors
    /// `ProviderResolution` for an unknown provider id.
    pub fn resolve(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedProvider, DomainError> {
        let entry = self.entry(provider_id)?;
        let storage = if entry.storage_kind.is_some() || entry.rag_provider.is_some() {
            Some(self.resolve_storage(provider_id, tenant_id)?)
        } else {
            None
        };
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
            storage,
        })
    }

    /// The entry serving files / vector stores for `provider_id`: its
    /// `rag_provider` when set, else the provider itself.
    ///
    /// # Errors
    /// `ProviderResolution` for an unknown provider or one without storage.
    pub fn resolve_storage(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedStorage, DomainError> {
        let entry = self.entry(provider_id)?;
        let storage_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        let target = self.entry(storage_id)?;
        storage_of(storage_id, target, tenant_id)
    }

    /// Map a stored `storage_backend` label back to its storage entry (cleanup
    /// path). The alias is the tenant's override when one exists, exactly as in
    /// [`Self::resolve_storage`], so deletes reach the upstream that created the
    /// file / vector store.
    ///
    /// # Errors
    /// `ProviderResolution` when no storage-capable entry has this label.
    pub fn storage_for_backend_label(
        &self,
        label: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedStorage, DomainError> {
        let mut matches: Vec<(&String, &ProviderEntry)> = self
            .providers
            .iter()
            .filter(|(id, e)| e.storage_kind.is_some() && e.storage_backend_label(id) == label)
            .collect();
        matches.sort_by(|a, b| a.0.cmp(b.0));
        let (id, entry) = matches.first().ok_or_else(|| {
            DomainError::ProviderResolution(format!("no storage provider for backend '{label}'"))
        })?;
        storage_of(id, entry, tenant_id)
    }

    /// The knowledge-search target of `provider_id` (`knowledge_search.provider_id`)
    /// for `tenant_id` (DESIGN section 4 "Knowledge Search"): the entry must be
    /// of kind `openai_responses` or `anthropic_messages` and have a non-empty
    /// `api_version`; the alias is the tenant's.
    ///
    /// # Errors
    /// `ProviderResolution` when the entry is unknown or not usable.
    pub fn knowledge_target(
        &self,
        provider_id: &str,
        vector_store_id: &str,
        tenant_id: Uuid,
    ) -> Result<KnowledgeTarget, DomainError> {
        let entry = self.entry(provider_id)?;
        if !matches!(
            entry.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            return Err(DomainError::ProviderResolution(format!(
                "knowledge provider '{provider_id}' must be openai_responses or anthropic_messages"
            )));
        }
        let api_version = entry
            .api_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                DomainError::ProviderResolution(format!(
                    "knowledge provider '{provider_id}' has no api_version"
                ))
            })?;
        Ok(KnowledgeTarget {
            alias: alias_for(entry, tenant_id),
            api_version: api_version.to_owned(),
            vector_store_id: vector_store_id.to_owned(),
        })
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderEntry, DomainError> {
        self.providers.get(provider_id).ok_or_else(|| {
            DomainError::ProviderResolution(format!("unknown provider '{provider_id}'"))
        })
    }
}

fn storage_of(
    id: &str,
    entry: &ProviderEntry,
    tenant_id: Uuid,
) -> Result<ResolvedStorage, DomainError> {
    let kind = entry.storage_kind.ok_or_else(|| {
        DomainError::ProviderResolution(format!("provider '{id}' has no storage"))
    })?;
    Ok(ResolvedStorage {
        provider_id: id.to_owned(),
        kind,
        alias: alias_for(entry, tenant_id),
        api_version: entry.api_version.clone(),
        backend_label: entry.storage_backend_label(id),
    })
}

/// The tenant override's alias when one exists for `tenant_id`, else the entry's.
fn alias_for(entry: &ProviderEntry, tenant_id: Uuid) -> String {
    entry
        .tenant_overrides
        .iter()
        .find(|(key, _)| Uuid::parse_str(key).is_ok_and(|k| k == tenant_id))
        .and_then(|(_, ov)| ov.upstream_alias.clone())
        .unwrap_or_else(|| entry.alias().to_owned())
}

#[cfg(test)]
#[path = "resolver_tests.rs"]
mod resolver_tests;
