//! Provider resolution: catalog `provider_id` (+ tenant override) → adapter kind and OAGW alias.

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::DomainError;

/// A provider entry resolved for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
    pub storage_kind: StorageKind,
    pub api_version: Option<String>,
    pub storage_backend: String,
}

impl ResolvedProvider {
    /// RAG route prefix: `/v1` (`OpenAI`) or `/openai` (Azure).
    #[must_use]
    pub const fn rag_prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Appends `api-version` for Azure storage requests.
    #[must_use]
    pub fn rag_path(&self, suffix: &str) -> String {
        let base = format!("{}{suffix}", self.rag_prefix());
        match (self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => {
                let sep = if base.contains('?') { '&' } else { '?' };
                format!("{base}{sep}api-version={v}")
            }
            _ => base,
        }
    }

    /// Chat path with `{model}` substituted.
    #[must_use]
    pub fn chat_path(&self, provider_model_id: &str) -> String {
        self.api_path.replace("{model}", provider_model_id)
    }
}

/// Registry built from the gear configuration (aliases already filled).
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    entries: HashMap<String, ProviderEntry>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new(entries: HashMap<String, ProviderEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &HashMap<String, ProviderEntry> {
        &self.entries
    }

    /// Resolves the chat provider for a tenant.
    ///
    /// # Errors
    /// Internal error when the provider id is unknown.
    pub fn resolve(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedProvider, DomainError> {
        let e = self
            .entries
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("unknown provider '{provider_id}'")))?;
        let mut alias = e.effective_alias().to_owned();
        if let Some(o) = e.tenant_overrides.get(&tenant_id.to_string()) {
            if let Some(a) = o.upstream_alias.as_deref().filter(|a| !a.is_empty()) {
                a.clone_into(&mut alias);
            } else if let Some(h) = &o.host {
                alias.clone_from(h);
            }
        }
        Ok(ResolvedProvider {
            id: provider_id.to_owned(),
            kind: e.kind,
            alias,
            api_path: e.effective_api_path().to_owned(),
            storage_kind: e.storage_kind.unwrap_or(StorageKind::Openai),
            api_version: e.api_version.clone(),
            storage_backend: e
                .storage_backend
                .clone()
                .unwrap_or_else(|| provider_id.to_owned()),
        })
    }

    /// Resolves the storage (RAG) provider: `rag_provider` when set, else the provider itself.
    ///
    /// # Errors
    /// Internal error when the provider id is unknown.
    pub fn resolve_storage(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedProvider, DomainError> {
        let e = self
            .entries
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("unknown provider '{provider_id}'")))?;
        match &e.rag_provider {
            Some(rag) => self.resolve(rag, tenant_id),
            None => self.resolve(provider_id, tenant_id),
        }
    }

    /// Resolves a storage backend label (stored on attachments) back to a provider.
    ///
    /// # Errors
    /// Internal error when no provider uses the label.
    pub fn resolve_backend(
        &self,
        label: &str,
        tenant_id: Uuid,
    ) -> Result<ResolvedProvider, DomainError> {
        let id = self
            .entries
            .iter()
            .find(|(id, e)| e.storage_backend.as_deref().unwrap_or(id.as_str()) == label)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| DomainError::internal(format!("unknown storage backend '{label}'")))?;
        self.resolve(&id, tenant_id)
    }
}
