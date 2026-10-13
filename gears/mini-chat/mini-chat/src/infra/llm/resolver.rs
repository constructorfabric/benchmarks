//! Provider resolution: catalog `provider_id` (+ tenant override) → adapter kind and OAGW alias.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::DomainError;

/// Chat target of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTarget {
    /// Provider id.
    pub provider_id: String,
    /// Adapter kind.
    pub kind: ProviderKind,
    /// OAGW alias.
    pub alias: String,
    /// Chat path (`{model}` not yet substituted, query kept).
    pub api_path: String,
}

impl ChatTarget {
    /// Proxy URI for a provider model.
    #[must_use]
    pub fn uri(&self, provider_model_id: &str) -> String {
        format!("/{}{}", self.alias, self.api_path.replace("{model}", provider_model_id))
    }
}

/// File / vector-store target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// Provider id of the storage provider.
    pub provider_id: String,
    /// Storage implementation.
    pub kind: StorageKind,
    /// OAGW alias.
    pub alias: String,
    /// Azure `api-version`.
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend`.
    pub backend: String,
}

impl StorageTarget {
    /// Proxy URI for a RAG path such as `/files` or `/vector_stores/{id}`.
    #[must_use]
    pub fn uri(&self, path: &str) -> String {
        match self.kind {
            StorageKind::Openai => format!("/{}/v1{path}", self.alias),
            StorageKind::Azure => {
                let sep = if path.contains('?') { '&' } else { '?' };
                format!(
                    "/{}/openai{path}{sep}api-version={}",
                    self.alias,
                    self.api_version.as_deref().unwrap_or_default()
                )
            }
        }
    }
}

/// Upstream registration unit (provider entry or tenant override).
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    /// Provider id.
    pub provider_id: String,
    /// Tenant (override) or `None` for the entry itself.
    pub tenant: Option<String>,
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Plain HTTP.
    pub use_http: bool,
    /// Alias passed to OAGW (`None` = let OAGW derive it).
    pub requested_alias: Option<String>,
    /// Alias used for proxy calls.
    pub alias: String,
    /// Auth plugin.
    pub auth_plugin_type: Option<String>,
    /// Auth config.
    pub auth_config: Option<HashMap<String, String>>,
    /// Chat path.
    pub api_path: String,
    /// Storage kind (RAG routes).
    pub storage_kind: Option<StorageKind>,
}

/// Computes the alias for a host / port / configured alias (OAGW alias rules).
#[must_use]
pub fn alias_for(host: &str, port: u16, configured: Option<&str>) -> (Option<String>, String) {
    if let Some(a) = configured.filter(|a| !a.is_empty()) {
        return (Some(a.to_owned()), a.to_ascii_lowercase());
    }
    let is_ip = host.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>().is_ok();
    if is_ip || port == 80 || port == 443 {
        (Some(host.to_owned()), host.to_ascii_lowercase())
    } else {
        (None, format!("{}:{port}", host.to_ascii_lowercase()))
    }
}

/// OAGW upstream aliases keyed by `(provider_id, tenant override)`.
type AliasMap = HashMap<(String, Option<String>), String>;

/// Provider resolver.
pub struct ProviderResolver {
    entries: HashMap<String, ProviderEntry>,
    aliases: RwLock<AliasMap>,
}

impl ProviderResolver {
    /// Builds the resolver from configured entries.
    #[must_use]
    pub fn new(entries: HashMap<String, ProviderEntry>) -> Self {
        let r = Self { entries, aliases: RwLock::new(HashMap::new()) };
        let specs = r.upstream_specs();
        if let Ok(mut a) = r.aliases.write() {
            for s in specs {
                a.insert((s.provider_id.clone(), s.tenant.clone()), s.alias.clone());
            }
        }
        r
    }

    /// Provider entries.
    #[must_use]
    pub fn entries(&self) -> &HashMap<String, ProviderEntry> {
        &self.entries
    }

    /// Records the alias OAGW used for an upstream.
    pub fn set_alias(&self, provider_id: &str, tenant: Option<&str>, alias: &str) {
        if let Ok(mut a) = self.aliases.write() {
            a.insert((provider_id.to_owned(), tenant.map(str::to_owned)), alias.to_owned());
        }
    }

    /// Every upstream to provision.
    #[must_use]
    pub fn upstream_specs(&self) -> Vec<UpstreamSpec> {
        let mut out = Vec::new();
        let mut ids: Vec<&String> = self.entries.keys().collect();
        ids.sort();
        for id in ids {
            let e = &self.entries[id];
            let port = e.effective_port();
            let (requested, alias) = alias_for(&e.host, port, e.upstream_alias.as_deref());
            out.push(UpstreamSpec {
                provider_id: id.clone(),
                tenant: None,
                host: e.host.clone(),
                port,
                use_http: e.use_http,
                requested_alias: requested,
                alias,
                auth_plugin_type: e.auth_plugin_type.clone(),
                auth_config: e.auth_config.clone(),
                api_path: e.api_path.clone(),
                storage_kind: e.storage_kind,
            });
            let mut tids: Vec<&String> = e.tenant_overrides.keys().collect();
            tids.sort();
            for tid in tids {
                let o = &e.tenant_overrides[tid];
                let host = o.host.clone().unwrap_or_else(|| e.host.clone());
                let (requested, alias) = alias_for(&host, port, o.upstream_alias.as_deref());
                out.push(UpstreamSpec {
                    provider_id: id.clone(),
                    tenant: Some(tid.clone()),
                    host,
                    port,
                    use_http: e.use_http,
                    requested_alias: requested,
                    alias,
                    auth_plugin_type: o.auth_plugin_type.clone().or_else(|| e.auth_plugin_type.clone()),
                    auth_config: o.auth_config.clone().or_else(|| e.auth_config.clone()),
                    api_path: e.api_path.clone(),
                    storage_kind: e.storage_kind,
                });
            }
        }
        out
    }

    fn alias(&self, provider_id: &str, tenant_id: Uuid) -> Option<String> {
        let a = self.aliases.read().ok()?;
        let entry = self.entries.get(provider_id)?;
        let tid = tenant_id.to_string();
        if entry.tenant_overrides.contains_key(&tid)
            && let Some(alias) = a.get(&(provider_id.to_owned(), Some(tid)))
        {
            return Some(alias.clone());
        }
        a.get(&(provider_id.to_owned(), None)).cloned()
    }

    /// Chat target for a catalog provider id and tenant.
    ///
    /// # Errors
    /// Internal when the provider is not configured.
    pub fn chat_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<ChatTarget, DomainError> {
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{provider_id}' is not configured")))?;
        let alias = self
            .alias(provider_id, tenant_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{provider_id}' has no alias")))?;
        Ok(ChatTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias,
            api_path: entry.api_path.clone(),
        })
    }

    /// Storage target for the provider serving a chat model (its `rag_provider` or itself).
    ///
    /// # Errors
    /// Internal when no storage-capable provider is configured.
    pub fn storage_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{provider_id}' is not configured")))?;
        let rag_id = entry.rag_provider.clone().unwrap_or_else(|| provider_id.to_owned());
        self.storage_target_for(&rag_id, tenant_id)
    }

    fn storage_target_for(&self, rag_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let rag = self
            .entries
            .get(rag_id)
            .ok_or_else(|| DomainError::internal(format!("rag provider '{rag_id}' is not configured")))?;
        let kind = rag
            .storage_kind
            .ok_or_else(|| DomainError::internal(format!("provider '{rag_id}' has no storage_kind")))?;
        let alias = self
            .alias(rag_id, tenant_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{rag_id}' has no alias")))?;
        Ok(StorageTarget {
            provider_id: rag_id.to_owned(),
            kind,
            alias,
            api_version: rag.api_version.clone(),
            backend: rag.storage_backend.clone().unwrap_or_else(|| rag_id.to_owned()),
        })
    }

    /// Storage target by the stored backend label (cleanup).
    ///
    /// # Errors
    /// Internal when no provider has that label.
    pub fn storage_target_by_backend(&self, backend: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let id = self
            .entries
            .iter()
            .find(|(id, e)| e.storage_backend.as_deref().unwrap_or(id.as_str()) == backend)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| DomainError::internal(format!("no provider for storage backend '{backend}'")))?;
        self.storage_target_for(&id, tenant_id)
    }
}
