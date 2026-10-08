//! Provider resolution (spec §11.1): catalog `provider_id` + tenant → adapter
//! kind, OAGW alias and request path.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::{DomainError, DomainResult};

/// A provider entry resolved for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// OAGW upstream alias (tenant override applied).
    pub alias: String,
    /// Chat request path; may contain `{model}`.
    pub api_path: String,
    pub storage_kind: Option<StorageKind>,
    pub storage_backend: String,
    pub api_version: Option<String>,
    /// Tenant the provider was resolved for (selects the tenant override).
    pub tenant_id: Uuid,
}

impl ResolvedProvider {
    /// `proxy_request` URI of a chat request: `/{alias}{api_path}` with
    /// `{model}` replaced by `provider_model_id`.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        let path = self.api_path.replace("{model}", provider_model_id);
        if path.starts_with('/') {
            format!("/{}{path}", self.alias)
        } else {
            format!("/{}/{path}", self.alias)
        }
    }
}

/// Upstream of a provider entry (`None`) or of one of its tenant overrides.
type AliasKey = (String, Option<Uuid>);

/// Resolves providers from the gear configuration.
#[derive(Debug)]
pub struct ProviderResolver {
    providers: HashMap<String, ProviderEntry>,
    /// Aliases OAGW assigned at provisioning that differ from the configured
    /// ones (an auto-derived `host:port` alias).
    alias_overrides: RwLock<HashMap<AliasKey, String>>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(cfg: &MiniChatConfig) -> Self {
        Self {
            providers: cfg.providers.clone(),
            alias_overrides: RwLock::new(HashMap::new()),
        }
    }

    /// Route `provider_id` (its entry when `tenant` is `None`, else that tenant
    /// override) by `alias`: the alias OAGW registered the upstream under.
    pub fn set_alias_override(&self, provider_id: &str, tenant: Option<Uuid>, alias: &str) {
        self.alias_overrides
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((provider_id.to_owned(), tenant), alias.to_owned());
    }

    fn alias_override(&self, provider_id: &str, tenant: Option<Uuid>) -> Option<String> {
        self.alias_overrides
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(provider_id.to_owned(), tenant))
            .cloned()
    }

    /// Provider `provider_id` for `tenant_id`.
    ///
    /// # Errors
    /// `Internal` when the provider is not configured.
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> DomainResult<ResolvedProvider> {
        let entry = self.entry(provider_id)?;
        let alias = match entry.tenant_overrides.get(&tenant_id) {
            Some(ov) => self
                .alias_override(provider_id, Some(tenant_id))
                .or_else(|| ov.upstream_alias.clone().or_else(|| ov.host.clone())),
            None => self.alias_override(provider_id, None),
        }
        .unwrap_or_else(|| entry.effective_upstream_alias().to_owned());
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias,
            api_path: entry.api_path.clone(),
            storage_kind: Some(entry.storage_kind),
            storage_backend: entry
                .storage_backend
                .clone()
                .unwrap_or_else(|| provider_id.to_owned()),
            api_version: entry.api_version.clone().filter(|v| !v.trim().is_empty()),
            tenant_id,
        })
    }

    /// Provider serving files / vector stores for `provider_id` (its
    /// `rag_provider`, else itself) for `tenant_id`.
    ///
    /// # Errors
    /// `Internal` when a provider is not configured.
    pub fn resolve_rag(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> DomainResult<ResolvedProvider> {
        let rag_id = self.entry(provider_id)?.rag_provider_id(provider_id);
        self.resolve(rag_id, tenant_id)
    }

    /// Provider behind the storage label `backend` (`attachments.storage_backend`,
    /// `chat_vector_stores.provider`) for `tenant_id`: the entry whose effective
    /// `storage_backend` equals `backend` (the lowest provider id when several
    /// do), else the provider whose id is `backend`.
    ///
    /// # Errors
    /// `Internal` when no provider matches.
    pub fn resolve_storage(
        &self,
        backend: &str,
        tenant_id: Uuid,
    ) -> DomainResult<ResolvedProvider> {
        let by_backend = self
            .providers
            .iter()
            .filter(|(id, entry)| entry.storage_backend.as_deref().unwrap_or(id) == backend)
            .map(|(id, _)| id)
            .min();
        match by_backend {
            Some(id) => self.resolve(id, tenant_id),
            None => self.resolve(backend, tenant_id),
        }
    }

    /// The provider of kind `kind` with the lowest id, for `tenant_id` (the
    /// fallback owner of Anthropic secondary copies when the chat's model no
    /// longer names one); `None` when no entry has that kind.
    #[must_use]
    pub fn resolve_kind(&self, kind: ProviderKind, tenant_id: Uuid) -> Option<ResolvedProvider> {
        let id = self
            .providers
            .iter()
            .filter(|(_, e)| e.kind == kind)
            .map(|(id, _)| id)
            .min()?;
        self.resolve(id, tenant_id).ok()
    }

    /// The provider of kind `kind` whose upstream alias for `tenant_id` is
    /// `alias` (the lowest id when several are); `None` when none is.
    #[must_use]
    pub fn resolve_by_alias(
        &self,
        kind: ProviderKind,
        alias: &str,
        tenant_id: Uuid,
    ) -> Option<ResolvedProvider> {
        let mut ids: Vec<&String> = self
            .providers
            .iter()
            .filter(|(_, e)| e.kind == kind)
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| self.resolve(id, tenant_id).ok())
            .find(|p| p.alias == alias)
    }

    /// Some entry is of kind `kind`.
    #[must_use]
    pub fn has_kind(&self, kind: ProviderKind) -> bool {
        self.providers.values().any(|e| e.kind == kind)
    }

    fn entry(&self, provider_id: &str) -> DomainResult<&ProviderEntry> {
        self.providers.get(provider_id).ok_or_else(|| {
            DomainError::internal(format!("provider '{provider_id}' is not configured"))
        })
    }
}

#[cfg(test)]
#[path = "resolver_tests.rs"]
mod resolver_tests;
