//! Provider resolution: catalog `provider_id` (+ tenant overrides) → adapter
//! kind and OAGW upstream alias.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// A resolved chat provider for one request.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

/// A resolved file / vector-store backend.
#[derive(Debug, Clone)]
pub struct StorageTarget {
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend_label: String,
    pub kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
}

impl StorageTarget {
    /// Path prefix of the Files / Vector Stores API.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Query string (`?api-version=…` for Azure) appended to RAG requests.
    #[must_use]
    pub fn query(&self) -> String {
        match (self.kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => format!("?api-version={v}"),
            _ => String::new(),
        }
    }
}

/// Registry of configured providers (aliases filled at gear init).
#[derive(Debug)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderEntry>,
    /// Alias actually registered in OAGW when it differs from the configured
    /// one (keyed by configured alias).
    alias_overrides: RwLock<HashMap<String, String>>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new(providers: BTreeMap<String, ProviderEntry>) -> Self {
        Self {
            providers,
            alias_overrides: RwLock::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, ProviderEntry> {
        &self.providers
    }

    #[must_use]
    pub fn get(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    /// Record that OAGW registered `configured` under `actual`.
    pub fn set_actual_alias(&self, configured: &str, actual: &str) {
        if configured != actual
            && let Ok(mut m) = self.alias_overrides.write()
        {
            m.insert(configured.to_owned(), actual.to_owned());
        }
    }

    fn effective_alias(&self, configured: &str) -> String {
        self.alias_overrides
            .read()
            .ok()
            .and_then(|m| m.get(configured).cloned())
            .unwrap_or_else(|| configured.to_owned())
    }

    fn alias_for(&self, entry: &ProviderEntry, tenant_id: Uuid) -> String {
        let tenant_key = tenant_id.to_string();
        let configured = entry
            .tenant_overrides
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&tenant_key))
            .and_then(|(_, o)| o.upstream_alias.clone().or_else(|| o.host.clone()))
            .or_else(|| entry.upstream_alias.clone())
            .unwrap_or_else(|| entry.host.clone());
        self.effective_alias(&configured)
    }

    /// Resolve the chat provider of a catalog model.
    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Option<ResolvedProvider> {
        let entry = self.providers.get(provider_id)?;
        Some(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: self.alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
        })
    }

    /// Resolve the storage backend of a provider (`rag_provider` or itself).
    #[must_use]
    pub fn storage_for(&self, provider_id: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let entry = self.providers.get(provider_id)?;
        let storage_id = entry
            .rag_provider
            .clone()
            .unwrap_or_else(|| provider_id.to_owned());
        self.storage_target(&storage_id, tenant_id)
    }

    fn storage_target(&self, storage_id: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let e = self.providers.get(storage_id)?;
        let kind = e.storage_kind?;
        Some(StorageTarget {
            backend_label: e
                .storage_backend
                .clone()
                .unwrap_or_else(|| storage_id.to_owned()),
            kind,
            alias: self.alias_for(e, tenant_id),
            api_version: e.api_version.clone(),
        })
    }

    /// Storage target by the label stored on attachment rows.
    #[must_use]
    pub fn storage_by_backend(&self, label: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let id = self
            .providers
            .iter()
            .find(|(id, e)| {
                e.storage_kind.is_some()
                    && e.storage_backend.as_deref().unwrap_or(id.as_str()) == label
            })
            .map(|(id, _)| id.clone())?;
        self.storage_target(&id, tenant_id)
    }

    /// The Anthropic provider entry (secondary image copies), if any.
    #[must_use]
    pub fn anthropic_alias(&self, provider_id: &str, tenant_id: Uuid) -> Option<String> {
        let e = self.providers.get(provider_id)?;
        (e.kind == ProviderKind::AnthropicMessages).then(|| self.alias_for(e, tenant_id))
    }
}

/// `{tenant_hex}{user_hex}` provider `user` value (64 characters).
#[must_use]
pub fn provider_user(tenant_id: Uuid, user_id: Uuid) -> String {
    format!("{}{}", tenant_id.as_simple(), user_id.as_simple())
}
