//! Provider registry: maps a catalog model's `provider_id` (and the tenant,
//! through `tenant_overrides`) to an adapter kind and an OAGW upstream alias,
//! and resolves the storage-capable provider (`rag_provider`) used for file
//! and vector-store operations.

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// Chat endpoint of a resolved provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    /// `api_path` with the `{model}` placeholder still present.
    pub api_path: String,
}

impl ChatTarget {
    /// Proxy URI `/{alias}{api_path}` with `{model}` substituted.
    #[must_use]
    pub fn uri(&self, provider_model_id: &str) -> String {
        format!(
            "/{}{}",
            self.alias,
            self.api_path.replace("{model}", provider_model_id)
        )
    }
}

/// File / vector-store endpoint of a resolved storage provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub provider_id: String,
    pub alias: String,
    pub storage_kind: StorageKind,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` /
    /// `chat_vector_stores.provider`.
    pub backend_label: String,
}

impl StorageTarget {
    /// RAG path prefix: `/v1` (openai) or `/openai` (azure).
    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Proxy URI for a RAG sub-path (e.g. `/files`), with `api-version` for
    /// Azure.
    #[must_use]
    pub fn uri(&self, sub_path: &str) -> String {
        let base = format!("/{}{}{}", self.alias, self.prefix(), sub_path);
        match (&self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => format!("{base}?api-version={v}"),
            _ => base,
        }
    }
}

/// Default upstream alias of a host, mirroring OAGW alias derivation: the
/// host on a standard port, `host:port` on a non-standard port (OAGW
/// requires that form for hostnames; for IP literals it keeps upstreams on
/// different ports apart).
#[must_use]
pub fn default_alias(host: &str, port: u16) -> String {
    if port == 80 || port == 443 {
        host.to_ascii_lowercase()
    } else {
        format!("{}:{port}", host.to_ascii_lowercase())
    }
}

/// Provider entry with its effective alias resolved.
#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    pub id: String,
    pub entry: ProviderEntry,
    pub alias: String,
    /// Tenant id → effective alias of the tenant override.
    pub tenant_aliases: HashMap<Uuid, String>,
}

/// Immutable provider registry built at gear init.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    entries: HashMap<String, ResolvedEntry>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new(providers: &HashMap<String, ProviderEntry>) -> Self {
        let mut entries = HashMap::new();
        for (id, entry) in providers {
            let port = entry.effective_port();
            let alias = entry
                .upstream_alias
                .clone()
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| default_alias(&entry.host, port));
            let mut tenant_aliases = HashMap::new();
            for (tenant, ov) in &entry.tenant_overrides {
                let Ok(tid) = Uuid::parse_str(tenant) else {
                    continue;
                };
                let ov_alias = ov
                    .upstream_alias
                    .clone()
                    .filter(|a| !a.trim().is_empty())
                    .or_else(|| ov.host.as_deref().map(|h| default_alias(h, port)))
                    .unwrap_or_else(|| alias.clone());
                tenant_aliases.insert(tid, ov_alias);
            }
            entries.insert(
                id.clone(),
                ResolvedEntry {
                    id: id.clone(),
                    entry: entry.clone(),
                    alias,
                    tenant_aliases,
                },
            );
        }
        Self { entries }
    }

    /// All entries (sorted by id) for provisioning.
    #[must_use]
    pub fn entries(&self) -> Vec<&ResolvedEntry> {
        let mut v: Vec<&ResolvedEntry> = self.entries.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    #[must_use]
    pub fn get(&self, provider_id: &str) -> Option<&ResolvedEntry> {
        self.entries.get(provider_id)
    }

    fn alias_for(entry: &ResolvedEntry, tenant_id: Uuid) -> String {
        entry
            .tenant_aliases
            .get(&tenant_id)
            .cloned()
            .unwrap_or_else(|| entry.alias.clone())
    }

    /// Chat endpoint of a provider for a tenant.
    ///
    /// # Errors
    /// Unknown provider id.
    pub fn chat_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<ChatTarget, String> {
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| format!("unknown provider '{provider_id}'"))?;
        Ok(ChatTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.entry.kind,
            alias: Self::alias_for(entry, tenant_id),
            api_path: entry.entry.api_path.clone(),
        })
    }

    /// Storage endpoint for chats served by `provider_id`: the provider
    /// itself, or its `rag_provider`.
    ///
    /// # Errors
    /// Unknown provider or no storage-capable provider.
    pub fn storage_target(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<StorageTarget, String> {
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| format!("unknown provider '{provider_id}'"))?;
        let storage_entry = match &entry.entry.rag_provider {
            Some(rag) => self
                .entries
                .get(rag)
                .ok_or_else(|| format!("unknown rag_provider '{rag}'"))?,
            None => entry,
        };
        Self::storage_of(storage_entry, tenant_id)
    }

    fn storage_of(entry: &ResolvedEntry, tenant_id: Uuid) -> Result<StorageTarget, String> {
        let storage_kind = entry
            .entry
            .storage_kind
            .ok_or_else(|| format!("provider '{}' has no storage_kind", entry.id))?;
        Ok(StorageTarget {
            provider_id: entry.id.clone(),
            alias: Self::alias_for(entry, tenant_id),
            storage_kind,
            api_version: entry.entry.api_version.clone(),
            backend_label: entry
                .entry
                .storage_backend
                .clone()
                .unwrap_or_else(|| entry.id.clone()),
        })
    }

    /// Storage endpoint by the stored backend label (cleanup path).
    #[must_use]
    pub fn storage_by_label(&self, label: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let mut entries: Vec<&ResolvedEntry> = self.entries.values().collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
            .into_iter()
            .filter(|e| e.entry.storage_kind.is_some())
            .find(|e| e.entry.storage_backend.as_deref().unwrap_or(&e.id) == label)
            .and_then(|e| Self::storage_of(e, tenant_id).ok())
    }

    /// Anthropic Files endpoint (secondary image copies) of a provider.
    #[must_use]
    pub fn anthropic_alias(&self, provider_id: &str, tenant_id: Uuid) -> Option<String> {
        let entry = self.entries.get(provider_id)?;
        (entry.entry.kind == ProviderKind::AnthropicMessages)
            .then(|| Self::alias_for(entry, tenant_id))
    }

    /// `true` when at least one entry uses the Anthropic adapter.
    #[must_use]
    pub fn has_anthropic(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.entry.kind == ProviderKind::AnthropicMessages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_alias_mirrors_oagw_derivation() {
        assert_eq!(default_alias("api.openai.com", 443), "api.openai.com");
        assert_eq!(default_alias("127.0.0.1", 8999), "127.0.0.1:8999");
        assert_eq!(default_alias("10.0.0.1", 443), "10.0.0.1");
        assert_eq!(default_alias("localhost", 8999), "localhost:8999");
        assert_eq!(default_alias("Example.COM", 80), "example.com");
    }

    #[test]
    fn storage_uri_adds_api_version_for_azure() {
        let t = StorageTarget {
            provider_id: "az".into(),
            alias: "az.example.com".into(),
            storage_kind: StorageKind::Azure,
            api_version: Some("2025-03-01-preview".into()),
            backend_label: "az".into(),
        };
        assert_eq!(
            t.uri("/files"),
            "/az.example.com/openai/files?api-version=2025-03-01-preview"
        );
        let o = StorageTarget {
            storage_kind: StorageKind::Openai,
            api_version: None,
            ..t
        };
        assert_eq!(o.uri("/vector_stores"), "/az.example.com/v1/vector_stores");
    }

    #[test]
    fn chat_uri_substitutes_model() {
        let t = ChatTarget {
            provider_id: "p".into(),
            kind: ProviderKind::OpenaiResponses,
            alias: "h".into(),
            api_path: "/openai/deployments/{model}/chat?api-version=1".into(),
        };
        assert_eq!(t.uri("gpt"), "/h/openai/deployments/gpt/chat?api-version=1");
    }
}
