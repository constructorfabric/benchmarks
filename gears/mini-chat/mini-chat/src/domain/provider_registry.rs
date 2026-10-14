//! Provider registry: the resolver from a catalog model's `provider_id` (and the tenant,
//! through `tenant_overrides`) to a provider entry and its OAGW alias.
//!
//! Slice S2, algorithm `provider-registry`. Built at init from the validated `providers`
//! map; credentials are held by reference only (`auth_config.secret_ref`).

use std::collections::BTreeMap;

use thiserror::Error;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::{
    ProviderConfig, ProviderKind, StorageKind, TenantOverride, default_upstream_alias,
    normalize_oagw_alias,
};
use crate::domain::error::DomainError;

/// One provider entry (or one tenant override of it) with every default applied and every
/// override field resolved against the entry.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProvider {
    /// The `providers.<id>` key.
    pub provider_id: String,
    pub kind: ProviderKind,
    pub host: String,
    /// `443`, or `80` when `use_http`, unless configured.
    pub port: u16,
    pub use_http: bool,
    /// The configured alias, else the OAGW-derived alias of the host (the bare host, plus
    /// `:port` for a hostname on a non-standard port); for an override: its own alias, else
    /// the alias of its own host, else the entry alias.
    pub upstream_alias: String,
    pub api_path: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: BTreeMap<String, String>,
    pub storage_kind: StorageKind,
    /// The configured label, else the provider id.
    pub storage_backend: String,
    pub api_version: Option<String>,
    pub rag_provider: Option<String>,
}

/// A provider entry and its tenant overrides, keyed by the tenant id text of the config.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub base: EffectiveProvider,
    pub overrides: BTreeMap<String, EffectiveProvider>,
}

impl ProviderEntry {
    fn of(id: &str, config: &ProviderConfig) -> Self {
        let base = EffectiveProvider::of_entry(id, config);
        let overrides = config
            .tenant_overrides
            .iter()
            .map(|(tenant, over)| (normalize_tenant_key(tenant), base.with_override(over)))
            .collect();
        Self { base, overrides }
    }
}

/// The tenant key as the resolver looks it up: hyphenated lowercase when it parses as a UUID,
/// verbatim otherwise.
fn normalize_tenant_key(key: &str) -> String {
    Uuid::parse_str(key).map_or_else(|_| key.to_owned(), |id| id.hyphenated().to_string())
}

impl EffectiveProvider {
    fn of_entry(id: &str, config: &ProviderConfig) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-entry-defaults
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-credentials-by-reference
        // `auth_config` carries `secret_ref` (a `cred://` name) and never a secret value
        let provider = Self {
            provider_id: id.to_owned(),
            kind: config.kind,
            host: config.host.clone(),
            port: config.effective_port(),
            use_http: config.use_http,
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-fill-alias
            // DESIGN fills the alias with the host; OAGW accepts only its derived alias on a
            // hostname endpoint (`host:port` on a non-standard port), so the default is that
            // alias. A configured alias was checked against it by the config validation.
            upstream_alias: config.upstream_alias.as_deref().map_or_else(
                || default_upstream_alias(&config.host, config.effective_port(), config.use_http),
                normalize_oagw_alias,
            ),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-fill-alias
            api_path: config.api_path.clone(),
            auth_plugin_type: config.auth_plugin_type.clone(),
            auth_config: config.auth_config.clone(),
            storage_kind: config.storage_kind,
            storage_backend: config
                .storage_backend
                .clone()
                .unwrap_or_else(|| id.to_owned()),
            api_version: config.api_version.clone(),
            rag_provider: config.rag_provider.clone(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-credentials-by-reference
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-entry-defaults
        provider
    }

    /// This entry with the fields of a tenant override applied; every field falls back to the
    /// entry on its own, and an unset alias is the override host.
    fn with_override(&self, over: &TenantOverride) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-override-fallback
        Self {
            host: over.host.clone().unwrap_or_else(|| self.host.clone()),
            upstream_alias: over
                .upstream_alias
                .as_deref()
                .map(normalize_oagw_alias)
                .or_else(|| {
                    over.host
                        .as_deref()
                        .map(|host| default_upstream_alias(host, self.port, self.use_http))
                })
                .unwrap_or_else(|| self.upstream_alias.clone()),
            auth_plugin_type: over
                .auth_plugin_type
                .clone()
                .or_else(|| self.auth_plugin_type.clone()),
            auth_config: over
                .auth_config
                .clone()
                .unwrap_or_else(|| self.auth_config.clone()),
            ..self.clone()
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-override-fallback
    }
}

/// The resolver cannot map a provider id (for example the entry was removed). Callers map it
/// to 500 `internal` before any stream opens.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderResolutionError {
    #[error("provider `{0}` is not configured")]
    UnknownProvider(String),
}

impl From<ProviderResolutionError> for DomainError {
    fn from(error: ProviderResolutionError) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-return-error
        // the diagnostic stays server-side; the boundary adapter projects a bare 500 `internal`
        Self::internal(format!("provider resolution failed: {error}"))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-return-error
    }
}

/// The provider resolver built at init.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    entries: BTreeMap<String, ProviderEntry>,
}

impl ProviderRegistry {
    /// Build the registry from the validated provider entries (defaults applied, alias filled
    /// with the OAGW-derived alias of the host, per-field override fallback).
    #[must_use]
    pub fn build(providers: &BTreeMap<String, ProviderConfig>) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-default-entry
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-default-values
        // an absent `providers` section was already replaced by the single default `openai`
        // entry when the configuration was parsed, so `providers` always holds the entries
        // to register here
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-default-values
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-default-entry
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-each-entry
        let entries = providers
            .iter()
            .map(|(id, config)| (id.clone(), ProviderEntry::of(id, config)))
            .collect();
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-each-entry
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-build-resolver
        let registry = Self { entries };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-build-resolver
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-return
        registry
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-return
    }

    /// The entry of `provider_id` with its overrides.
    #[must_use]
    pub fn entry(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.entries.get(provider_id)
    }

    /// Every entry in provider-id order.
    pub fn entries(&self) -> impl Iterator<Item = &ProviderEntry> {
        self.entries.values()
    }

    /// Resolve the provider of a catalog model for a tenant: the tenant's override (hyphenated
    /// lowercase tenant id as key) when one exists, else the entry itself.
    ///
    /// # Errors
    ///
    /// [`ProviderResolutionError::UnknownProvider`] when no entry has that id.
    pub fn resolve(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<&EffectiveProvider, ProviderResolutionError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-resolution-failed
        let entry = self
            .entries
            .get(provider_id)
            .ok_or_else(|| ProviderResolutionError::UnknownProvider(provider_id.to_owned()))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provider-registry:p1:inst-registry-resolution-failed
        Ok(entry
            .overrides
            .get(&tenant_id.hyphenated().to_string())
            .unwrap_or(&entry.base))
    }
}

#[cfg(test)]
#[path = "provider_registry_tests.rs"]
mod provider_registry_tests;
