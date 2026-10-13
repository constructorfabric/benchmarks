//! Provider resolution: catalog `provider_id` (+ tenant override) → adapter kind and OAGW alias.

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind};

/// A provider entry resolved for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// OAGW upstream alias (request URI is `/{alias}{path}`).
    pub alias: String,
    pub api_path: String,
    pub storage_kind: StorageKind,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub storage_backend: String,
}

impl ResolvedProvider {
    /// RAG route prefix: `/v1` (openai) or `/openai` (azure).
    #[must_use]
    pub const fn rag_prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Query suffix for RAG requests (`?api-version=` on azure).
    #[must_use]
    pub fn rag_query(&self) -> String {
        match (self.storage_kind, self.api_version.as_deref()) {
            (StorageKind::Azure, Some(v)) if !v.is_empty() => format!("?api-version={v}"),
            _ => String::new(),
        }
    }
}

/// Resolves provider entries (built at gear init after aliases were filled).
#[derive(Debug, Clone)]
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

    #[must_use]
    pub fn entry(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Resolves `provider_id` for `tenant_id` (tenant override host/alias wins).
    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Option<ResolvedProvider> {
        let p = self.providers.get(provider_id)?;
        let mut alias = p.alias();
        if let Some(o) = p.tenant_overrides.iter().find_map(|(k, v)| {
            Uuid::parse_str(k).ok().filter(|u| *u == tenant_id).map(|_| v)
        }) {
            if let Some(a) = o.upstream_alias.as_ref().filter(|a| !a.is_empty()) {
                alias.clone_from(a);
            } else if let Some(h) = &o.host {
                alias = crate::config::derive_alias(h, p.effective_port());
            }
        }
        Some(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: p.kind,
            alias,
            api_path: p.api_path.clone(),
            storage_kind: p.storage_kind,
            api_version: p.api_version.clone(),
            storage_backend: p.storage_backend_label(provider_id),
        })
    }

    /// Storage-capable provider id for a chat provider: its `rag_provider`, or itself.
    #[must_use]
    pub fn storage_provider_id(&self, provider_id: &str) -> Option<String> {
        let p = self.providers.get(provider_id)?;
        Some(p.rag_provider.clone().unwrap_or_else(|| provider_id.to_owned()))
    }

    /// Maps a stored `storage_backend` label back to a provider id.
    #[must_use]
    pub fn provider_for_storage_backend(&self, label: &str) -> Option<String> {
        if self.providers.contains_key(label) {
            return Some(label.to_owned());
        }
        let mut ids: Vec<&String> = self
            .providers
            .iter()
            .filter(|(id, p)| p.storage_backend_label(id) == label)
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        ids.first().map(|s| (*s).clone())
    }
}
