//! Provider registry: maps a catalog `provider_id` (and tenant) to the
//! provider entry, its adapter kind and the OAGW upstream alias.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// Alias OAGW uses for an endpoint: the configured alias, else the host; a
/// hostname with a non-standard port is derived as `host:port` by OAGW.
#[must_use]
pub fn effective_alias(configured: Option<&str>, host: &str, port: u16) -> String {
    if let Some(a) = configured
        && !a.trim().is_empty()
    {
        return a.trim().to_ascii_lowercase();
    }
    let is_ip = host.parse::<std::net::IpAddr>().is_ok()
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::Ipv6Addr>()
            .is_ok();
    if is_ip || port == 80 || port == 443 {
        host.to_ascii_lowercase()
    } else {
        format!("{}:{port}", host.to_ascii_lowercase())
    }
}

/// A provider resolved for a request (tenant overrides applied).
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
    pub storage_kind: Option<StorageKind>,
    pub api_version: Option<String>,
}

/// Storage (files / vector stores) target of a provider.
#[derive(Debug, Clone)]
pub struct ResolvedStorage {
    /// Provider id of the storage-capable entry.
    pub provider_id: String,
    /// Label stored in `attachments.storage_backend`.
    pub backend_label: String,
    pub alias: String,
    pub storage_kind: StorageKind,
    pub api_version: Option<String>,
}

impl ResolvedStorage {
    /// Path prefix of the Files / Vector Stores API.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Query string (with leading `?`) appended to RAG requests.
    #[must_use]
    pub fn query(&self) -> String {
        match (&self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => format!("?api-version={v}"),
            _ => String::new(),
        }
    }

    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("/{}{}{}{}", self.alias, self.prefix(), path, self.query())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    pub entries: BTreeMap<String, ProviderEntry>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn new(entries: BTreeMap<String, ProviderEntry>) -> Self {
        Self { entries }
    }

    fn override_for(
        e: &ProviderEntry,
        tenant: Option<Uuid>,
    ) -> Option<&crate::config::TenantOverride> {
        let t = tenant?;
        e.tenant_overrides.iter().find_map(|(k, v)| {
            (k.parse::<Uuid>().ok() == Some(t) || k.eq_ignore_ascii_case(&t.to_string()))
                .then_some(v)
        })
    }

    /// Alias used for an entry, considering the tenant override.
    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "kept as a method for call-site symmetry"
    )]
    pub fn alias_for(&self, e: &ProviderEntry, tenant: Option<Uuid>) -> String {
        let port = e.effective_port();
        if let Some(o) = Self::override_for(e, tenant) {
            let host = o.host.as_deref().unwrap_or(&e.host);
            return effective_alias(o.upstream_alias.as_deref(), host, port);
        }
        effective_alias(e.upstream_alias.as_deref(), &e.host, port)
    }

    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant: Option<Uuid>) -> Option<ResolvedProvider> {
        let e = self.entries.get(provider_id)?;
        Some(ResolvedProvider {
            id: provider_id.to_owned(),
            kind: e.kind,
            alias: self.alias_for(e, tenant),
            api_path: e.api_path.clone(),
            storage_kind: e.storage_kind,
            api_version: e.api_version.clone(),
        })
    }

    /// Storage target for a chat whose model is served by `provider_id`
    /// (`rag_provider` of the entry, or the entry itself).
    #[must_use]
    pub fn storage_for(&self, provider_id: &str, tenant: Option<Uuid>) -> Option<ResolvedStorage> {
        let e = self.entries.get(provider_id)?;
        let target_id = e
            .rag_provider
            .clone()
            .unwrap_or_else(|| provider_id.to_owned());
        self.storage_by_id(&target_id, tenant)
    }

    #[must_use]
    pub fn storage_by_id(
        &self,
        provider_id: &str,
        tenant: Option<Uuid>,
    ) -> Option<ResolvedStorage> {
        let e = self.entries.get(provider_id)?;
        let storage_kind = e.storage_kind?;
        Some(ResolvedStorage {
            provider_id: provider_id.to_owned(),
            backend_label: e
                .storage_backend
                .clone()
                .unwrap_or_else(|| provider_id.to_owned()),
            alias: self.alias_for(e, tenant),
            storage_kind,
            api_version: e.api_version.clone(),
        })
    }

    /// Map a stored backend label back to the storage target.
    #[must_use]
    pub fn storage_by_label(&self, label: &str, tenant: Option<Uuid>) -> Option<ResolvedStorage> {
        let id = self
            .entries
            .iter()
            .find(|(id, e)| {
                e.storage_backend.as_deref() == Some(label)
                    || (e.storage_backend.is_none() && id.as_str() == label)
            })
            .map(|(id, _)| id.clone())
            .or_else(|| self.entries.contains_key(label).then(|| label.to_owned()))?;
        self.storage_by_id(&id, tenant)
    }

    /// Whether any entry uses the Anthropic adapter.
    #[must_use]
    pub fn has_anthropic(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.kind == ProviderKind::AnthropicMessages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_rules() {
        assert_eq!(effective_alias(None, "127.0.0.1", 9999), "127.0.0.1");
        assert_eq!(
            effective_alias(None, "api.openai.com", 443),
            "api.openai.com"
        );
        assert_eq!(effective_alias(None, "localhost", 8080), "localhost:8080");
        assert_eq!(effective_alias(Some("Mock-A"), "localhost", 8080), "mock-a");
    }

    #[test]
    fn storage_urls() {
        let s = ResolvedStorage {
            provider_id: "azure".into(),
            backend_label: "azure".into(),
            alias: "h".into(),
            storage_kind: StorageKind::Azure,
            api_version: Some("2025-03-01-preview".into()),
        };
        assert_eq!(
            s.url("/files"),
            "/h/openai/files?api-version=2025-03-01-preview"
        );
        let o = ResolvedStorage {
            storage_kind: StorageKind::Openai,
            api_version: None,
            ..s
        };
        assert_eq!(o.url("/vector_stores/vs1"), "/h/v1/vector_stores/vs1");
    }
}
