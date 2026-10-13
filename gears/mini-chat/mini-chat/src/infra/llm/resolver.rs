//! Provider resolution: catalog `provider_id` (+ tenant) → adapter kind,
//! OAGW alias and paths; storage dispatch by `storage_kind` / `rag_provider`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use crate::config::{KnowledgeSearchConfig, ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::DomainError;

/// Resolved chat provider for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

impl ResolvedProvider {
    /// Proxy URI `/{alias}{api_path}` with `{model}` substituted.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        let path = self.api_path.replace("{model}", provider_model_id);
        format!("/{}{}", self.alias, path)
    }
}

/// Knowledge-search retrieval target of one request (DESIGN §4 "Knowledge Search").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeTarget {
    pub alias: String,
    pub api_version: String,
    pub vector_store_id: String,
    pub top_k: usize,
    pub max_chunk_chars: usize,
    pub max_calls: u32,
}

impl KnowledgeTarget {
    /// `POST /{alias}/openai/vector_stores/{id}/search?api-version={ver}`.
    #[must_use]
    pub fn search_uri(&self) -> String {
        format!(
            "/{}/openai/vector_stores/{}/search?api-version={}",
            self.alias, self.vector_store_id, self.api_version
        )
    }
}

/// Resolved file / vector-store target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub provider_id: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend_label: String,
}

impl StorageTarget {
    /// `{prefix}` of RAG routes.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Full proxy URI for a RAG sub-path (e.g. `/files`).
    #[must_use]
    pub fn uri(&self, sub: &str) -> String {
        let mut s = format!("/{}{}{}", self.alias, self.prefix(), sub);
        if self.storage_kind == StorageKind::Azure
            && let Some(v) = &self.api_version
        {
            s.push_str(if s.contains('?') { "&" } else { "?" });
            s.push_str("api-version=");
            s.push_str(v);
        }
        s
    }
}

/// Resolver over the configured provider entries.
pub struct ProviderResolver {
    providers: BTreeMap<String, ProviderEntry>,
    /// Configured alias → alias actually registered in OAGW.
    alias_overrides: RwLock<HashMap<String, String>>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: BTreeMap<String, ProviderEntry>) -> Arc<Self> {
        Arc::new(Self {
            providers,
            alias_overrides: RwLock::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn providers(&self) -> &BTreeMap<String, ProviderEntry> {
        &self.providers
    }

    /// Record that `configured` was registered in OAGW as `actual`.
    pub fn set_alias_override(&self, configured: &str, actual: &str) {
        if configured.eq_ignore_ascii_case(actual) {
            return;
        }
        if let Ok(mut m) = self.alias_overrides.write() {
            m.insert(configured.to_lowercase(), actual.to_owned());
        }
    }

    fn effective_alias(&self, configured: &str) -> String {
        self.alias_overrides
            .read()
            .ok()
            .and_then(|m| m.get(&configured.to_lowercase()).cloned())
            .unwrap_or_else(|| configured.to_owned())
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderEntry, DomainError> {
        self.providers.get(provider_id).ok_or_else(|| {
            DomainError::Internal(format!("provider '{provider_id}' is not configured"))
        })
    }

    fn alias_for(&self, entry: &ProviderEntry, tenant_id: &str) -> String {
        let configured = entry
            .tenant_overrides
            .get(tenant_id)
            .and_then(|o| o.upstream_alias.clone().or_else(|| o.host.clone()))
            .unwrap_or_else(|| entry.alias());
        self.effective_alias(&configured)
    }

    /// Chat provider for a catalog `provider_id` and tenant.
    ///
    /// # Errors
    /// Unknown provider id (internal).
    pub fn resolve(&self, provider_id: &str, tenant_id: &str) -> Result<ResolvedProvider, DomainError> {
        let entry = self.entry(provider_id)?;
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: self.alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
        })
    }

    /// Knowledge-search target for a request, or `None` (feature off, or the
    /// configured provider entry cannot serve it: kind, `api_version`, alias).
    #[must_use]
    // reason: flat sequence of eligibility checks with early `None` returns
    #[allow(clippy::cognitive_complexity)]
    pub fn knowledge_target(&self, cfg: &KnowledgeSearchConfig, tenant_id: &str) -> Option<KnowledgeTarget> {
        if !cfg.enabled {
            return None;
        }
        let provider_id = cfg.provider_id.as_deref()?;
        let vector_store_id = cfg.vector_store_id.clone()?;
        let Ok(entry) = self.entry(provider_id) else {
            tracing::warn!(provider_id, "knowledge search provider entry not found; knowledge search off");
            return None;
        };
        if !matches!(entry.kind, ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages) {
            tracing::warn!(provider_id, "knowledge search provider kind unsupported; knowledge search off");
            return None;
        }
        let api_version = entry.api_version.clone().unwrap_or_default();
        let alias = self.alias_for(entry, tenant_id);
        if api_version.trim().is_empty() || alias.is_empty() {
            tracing::warn!(provider_id, "knowledge search provider lacks api_version or alias; knowledge search off");
            return None;
        }
        Some(KnowledgeTarget {
            alias,
            api_version,
            vector_store_id,
            top_k: cfg.top_k,
            max_chunk_chars: cfg.max_chunk_chars,
            max_calls: cfg.max_calls_per_message,
        })
    }

    /// Storage provider id for a chat provider (`rag_provider` or itself).
    #[must_use]
    pub fn storage_provider_id(&self, provider_id: &str) -> String {
        self.providers
            .get(provider_id)
            .and_then(|e| e.rag_provider.clone())
            .unwrap_or_else(|| provider_id.to_owned())
    }

    /// File / vector-store target for a chat provider.
    ///
    /// # Errors
    /// The provider (or its `rag_provider`) has no `storage_kind`.
    pub fn storage(&self, provider_id: &str, tenant_id: &str) -> Result<StorageTarget, DomainError> {
        let sid = self.storage_provider_id(provider_id);
        self.storage_by_id(&sid, tenant_id)
    }

    /// Storage target for a storage provider id.
    ///
    /// # Errors
    /// Unknown provider or no `storage_kind`.
    pub fn storage_by_id(&self, sid: &str, tenant_id: &str) -> Result<StorageTarget, DomainError> {
        let entry = self.entry(sid)?;
        let kind = entry.storage_kind.ok_or_else(|| {
            DomainError::StorageUnavailable(format!("provider '{sid}' has no storage_kind"))
        })?;
        Ok(StorageTarget {
            provider_id: sid.to_owned(),
            storage_kind: kind,
            alias: self.alias_for(entry, tenant_id),
            api_version: entry.api_version.clone(),
            backend_label: entry.storage_backend.clone().unwrap_or_else(|| sid.to_owned()),
        })
    }

    /// Storage target by the label stored on attachment rows.
    ///
    /// # Errors
    /// No provider maps to the label.
    pub fn storage_by_label(&self, label: &str, tenant_id: &str) -> Result<StorageTarget, DomainError> {
        for (id, e) in &self.providers {
            if e.storage_kind.is_some()
                && e.storage_backend.as_deref().unwrap_or(id.as_str()) == label
            {
                return self.storage_by_id(id, tenant_id);
            }
        }
        Err(DomainError::Internal(format!(
            "no storage provider for backend '{label}'"
        )))
    }

    /// Whether `provider_id` is served by the Anthropic adapter.
    #[must_use]
    pub fn is_anthropic(&self, provider_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .is_some_and(|e| e.kind == ProviderKind::AnthropicMessages)
    }
}
