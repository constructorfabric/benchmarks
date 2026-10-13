//! Provider resolution: catalog `provider_id` (+ tenant override) →
//! adapter kind, OAGW alias and request path; storage dispatch by
//! `storage_kind` / `rag_provider`.

use std::collections::HashMap;
use std::sync::RwLock;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::errors::{DomainError, DomainResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    /// `api_path` with `{model}` replaced (may carry a query string).
    pub path: String,
}

impl ChatTarget {
    #[must_use]
    pub fn uri(&self) -> String {
        format!("/{}{}", self.alias, self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub provider_id: String,
    pub kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend: String,
}

impl StorageTarget {
    /// Path prefix of the files/vector-store API.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Full URI for `suffix` (e.g. `/files`), with `api-version` for Azure.
    #[must_use]
    pub fn uri(&self, suffix: &str) -> String {
        let base = format!("/{}{}{}", self.alias, self.prefix(), suffix);
        match (&self.kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => {
                let sep = if base.contains('?') { '&' } else { '?' };
                format!("{base}{sep}api-version={v}")
            }
            _ => base,
        }
    }
}

pub struct ProviderResolver {
    providers: HashMap<String, ProviderEntry>,
    /// Aliases actually registered with OAGW when they differ from the
    /// configured/derived ones, keyed by `(provider_id, tenant override)`.
    registered: RwLock<HashMap<(String, Option<String>), String>>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: HashMap<String, ProviderEntry>) -> Self {
        Self {
            providers,
            registered: RwLock::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn providers(&self) -> &HashMap<String, ProviderEntry> {
        &self.providers
    }

    /// Configured alias (`upstream_alias`, else host) for an entry or override.
    #[must_use]
    pub fn configured_alias(entry: &ProviderEntry, tenant_key: Option<&str>) -> String {
        if let Some(t) = tenant_key
            && let Some(ov) = entry.tenant_overrides.get(t)
        {
            if let Some(a) = &ov.upstream_alias {
                return a.clone();
            }
            if let Some(h) = &ov.host {
                return h.clone();
            }
        }
        entry.upstream_alias.clone().unwrap_or_else(|| entry.host.clone())
    }

    pub fn record_registered_alias(&self, provider_id: &str, tenant_key: Option<&str>, alias: &str) {
        if let Ok(mut m) = self.registered.write() {
            m.insert((provider_id.to_owned(), tenant_key.map(str::to_owned)), alias.to_owned());
        }
    }

    fn alias(&self, provider_id: &str, entry: &ProviderEntry, tenant: Uuid) -> String {
        let tkey = tenant.to_string();
        let tenant_key = entry.tenant_overrides.contains_key(&tkey).then_some(tkey);
        if let Ok(m) = self.registered.read()
            && let Some(a) = m.get(&(provider_id.to_owned(), tenant_key.clone()))
        {
            return a.clone();
        }
        Self::configured_alias(entry, tenant_key.as_deref())
    }

    fn entry(&self, provider_id: &str) -> DomainResult<&ProviderEntry> {
        self.providers
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{provider_id}' is not configured")))
    }

    /// Chat target of a model.
    ///
    /// # Errors
    /// 500 internal when the provider is not configured.
    pub fn chat_target(&self, provider_id: &str, tenant: Uuid, provider_model_id: &str) -> DomainResult<ChatTarget> {
        let entry = self.entry(provider_id)?;
        Ok(ChatTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: self.alias(provider_id, entry, tenant),
            path: entry.api_path.replace("{model}", provider_model_id),
        })
    }

    /// Storage target used for files / vector stores of a provider.
    ///
    /// # Errors
    /// 500 internal when neither the provider nor its `rag_provider` has a
    /// `storage_kind`.
    pub fn storage_target(&self, provider_id: &str, tenant: Uuid) -> DomainResult<StorageTarget> {
        let entry = self.entry(provider_id)?;
        let (sid, sentry) = match &entry.rag_provider {
            Some(rag) => (rag.as_str(), self.entry(rag)?),
            None => (provider_id, entry),
        };
        let kind = sentry
            .storage_kind
            .ok_or_else(|| DomainError::internal(format!("provider '{sid}' has no storage_kind")))?;
        Ok(StorageTarget {
            provider_id: sid.to_owned(),
            kind,
            alias: self.alias(sid, sentry, tenant),
            api_version: sentry.api_version.clone(),
            backend: sentry.storage_backend.clone().unwrap_or_else(|| sid.to_owned()),
        })
    }

    /// Storage target by the backend label stored on a row.
    ///
    /// # Errors
    /// 500 internal when no provider maps to the label.
    pub fn storage_by_backend(&self, backend: &str, tenant: Uuid) -> DomainResult<StorageTarget> {
        let found = self.providers.iter().find(|(id, e)| {
            e.storage_kind.is_some() && e.storage_backend.as_deref().unwrap_or(id.as_str()) == backend
        });
        match found {
            Some((id, _)) => self.storage_target(id, tenant),
            None => Err(DomainError::internal(format!("no storage provider for backend '{backend}'"))),
        }
    }
}
