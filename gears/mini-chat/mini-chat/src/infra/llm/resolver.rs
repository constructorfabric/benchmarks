//! Provider resolution: `provider_id` (+ tenant override) -> adapter, alias, paths.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// RAG (files / vector stores) target of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagTarget {
    pub provider_id: String,
    pub alias: String,
    pub storage_kind: StorageKind,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub storage_backend: String,
}

impl RagTarget {
    /// Path prefix of the files / vector-store API.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Full proxy URI for a RAG path such as `/files`.
    #[must_use]
    pub fn uri(&self, path: &str) -> String {
        let base = format!("/{}{}{}", self.alias, self.prefix(), path);
        match (self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => {
                let sep = if base.contains('?') { '&' } else { '?' };
                format!("{base}{sep}api-version={v}")
            }
            _ => base,
        }
    }
}

/// A resolved provider for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
    pub rag: Option<RagTarget>,
}

impl ResolvedProvider {
    /// Chat endpoint URI for a model.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        format!("/{}{}", self.alias, self.api_path.replace("{model}", provider_model_id))
    }
}

/// Resolves provider entries from configuration.
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    providers: BTreeMap<String, ProviderEntry>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: BTreeMap<String, ProviderEntry>) -> Self {
        Self { providers }
    }

    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, ProviderEntry> {
        &self.providers
    }

    fn alias_for(entry: &ProviderEntry, tenant_id: Uuid) -> String {
        if let Some(o) = entry.tenant_overrides.get(&tenant_id.to_string()) {
            if let Some(a) = o.upstream_alias.clone().filter(|a| !a.is_empty()) {
                return a;
            }
            if let Some(h) = o.host.clone().filter(|h| !h.is_empty()) {
                return h;
            }
        }
        entry.alias()
    }

    fn rag_for(&self, provider_id: &str, tenant_id: Uuid) -> Option<RagTarget> {
        let entry = self.providers.get(provider_id)?;
        let (rag_id, rag_entry) = match &entry.rag_provider {
            Some(r) => (r.as_str(), self.providers.get(r)?),
            None => (provider_id, entry),
        };
        let storage_kind = rag_entry.storage_kind?;
        Some(RagTarget {
            provider_id: rag_id.to_owned(),
            alias: Self::alias_for(rag_entry, tenant_id),
            storage_kind,
            api_version: rag_entry.api_version.clone(),
            storage_backend: rag_entry.storage_backend.clone().unwrap_or_else(|| rag_id.to_owned()),
        })
    }

    /// Resolves the provider of a catalog model for a tenant.
    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Option<ResolvedProvider> {
        let entry = self.providers.get(provider_id)?;
        Some(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: Self::alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
            rag: self.rag_for(provider_id, tenant_id),
        })
    }

    /// RAG target addressed by a stored `storage_backend` label.
    #[must_use]
    pub fn rag_by_backend(&self, backend: &str, tenant_id: Uuid) -> Option<RagTarget> {
        if self.providers.contains_key(backend) {
            if let Some(t) = self.rag_for(backend, tenant_id) {
                return Some(t);
            }
        }
        self.providers
            .iter()
            .find(|(id, e)| e.storage_backend.as_deref() == Some(backend) || id.as_str() == backend)
            .and_then(|(id, _)| self.rag_for(id, tenant_id))
    }
}
