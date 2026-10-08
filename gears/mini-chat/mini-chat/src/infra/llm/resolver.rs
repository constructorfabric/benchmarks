//! Provider resolution: catalog `provider_id` (+ tenant override) → OAGW alias and paths.

use std::net::IpAddr;
use std::sync::Arc;

use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderConfig, ProviderKind, StorageKind};
use crate::domain::error::DomainError;

/// A provider entry resolved for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// OAGW upstream alias (first path segment of proxied URIs).
    pub alias: String,
    pub api_path: String,
    pub storage_kind: Option<StorageKind>,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub storage_backend: String,
}

impl ResolvedProvider {
    /// Chat endpoint URI with `{model}` substituted.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        format!("/{}{}", self.alias, self.api_path.replace("{model}", provider_model_id))
    }

    /// RAG endpoint URI: `/{alias}{prefix}{suffix}` plus `api-version` for Azure.
    #[must_use]
    pub fn rag_uri(&self, suffix: &str) -> String {
        match self.storage_kind {
            Some(StorageKind::Azure) => {
                let ver = self.api_version.clone().unwrap_or_default();
                let sep = if suffix.contains('?') { '&' } else { '?' };
                format!("/{}/openai{suffix}{sep}api-version={ver}", self.alias)
            }
            _ => format!("/{}/v1{suffix}", self.alias),
        }
    }
}

/// Derives the OAGW alias of an endpoint: configured alias, else the host for IP hosts and
/// standard ports, else `host:port` (OAGW derives exactly this for hostname endpoints).
#[must_use]
pub fn derive_alias(host: &str, port: u16, use_http: bool, configured: Option<&str>) -> String {
    if let Some(a) = configured.filter(|a| !a.trim().is_empty()) {
        return a.to_owned();
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let is_ip = bare.parse::<IpAddr>().is_ok();
    let standard = if use_http { port == 80 } else { port == 443 };
    let host_lc = host.to_ascii_lowercase().trim_end_matches('.').to_owned();
    if is_ip || standard { host_lc } else { format!("{host_lc}:{port}") }
}

/// Resolver built once at init from the provider entries.
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    cfg: Arc<MiniChatConfig>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(cfg: Arc<MiniChatConfig>) -> Self {
        Self { cfg }
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderConfig, DomainError> {
        self.cfg
            .providers
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{provider_id}' is not configured")))
    }

    /// Resolves a provider entry for a tenant (applying `tenant_overrides`).
    ///
    /// # Errors
    /// Internal error when the provider id is not configured.
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ResolvedProvider, DomainError> {
        let p = self.entry(provider_id)?;
        let ov = p.tenant_overrides.get(&tenant_id.to_string());
        let host = ov.and_then(|o| o.host.clone()).unwrap_or_else(|| p.host.clone());
        let configured_alias = match ov {
            Some(o) => o.upstream_alias.clone().or_else(|| if o.host.is_some() { None } else { p.upstream_alias.clone() }),
            None => p.upstream_alias.clone(),
        };
        let alias = derive_alias(&host, p.effective_port(), p.use_http, configured_alias.as_deref());
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: p.kind,
            alias,
            api_path: p.api_path.clone(),
            storage_kind: p.storage_kind,
            api_version: p.api_version.clone(),
            storage_backend: p.storage_backend.clone().unwrap_or_else(|| provider_id.to_owned()),
        })
    }

    /// Provider used for file / vector-store operations of a chat model's provider
    /// (`rag_provider`, else the provider itself).
    ///
    /// # Errors
    /// Internal error when not configured.
    pub fn resolve_rag(&self, provider_id: &str, tenant_id: Uuid) -> Result<ResolvedProvider, DomainError> {
        let p = self.entry(provider_id)?;
        let rag_id = p.rag_provider.clone().unwrap_or_else(|| provider_id.to_owned());
        self.resolve(&rag_id, tenant_id)
    }

    /// Maps a stored `storage_backend` label back to a provider.
    ///
    /// # Errors
    /// Internal error when no provider uses the label.
    pub fn resolve_storage_backend(&self, label: &str, tenant_id: Uuid) -> Result<ResolvedProvider, DomainError> {
        let id = self
            .cfg
            .providers
            .iter()
            .find(|(id, p)| p.storage_backend.as_deref().unwrap_or(id.as_str()) == label)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| DomainError::internal(format!("no provider for storage backend '{label}'")))?;
        self.resolve(&id, tenant_id)
    }

    /// All provider ids.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        self.cfg.providers.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_rules() {
        assert_eq!(derive_alias("127.0.0.1", 9000, true, None), "127.0.0.1");
        assert_eq!(derive_alias("localhost", 9000, true, None), "localhost:9000");
        assert_eq!(derive_alias("api.openai.com", 443, false, None), "api.openai.com");
        assert_eq!(derive_alias("h", 1, false, Some("custom")), "custom");
    }
}
