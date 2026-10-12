//! Provider resolver (DESIGN section 3.2 `llm_provider`, ADR-0005).
//!
//! Built at initialization from the prepared configuration (upstream aliases
//! already filled), it maps a catalog model's `provider_id` and the request's
//! tenant to a [`ProviderTarget`]: the alias of the matching
//! `tenant_overrides.<tenant_id>` entry when there is one, otherwise the
//! entry's `upstream_alias` (the host when none was configured). An unknown
//! provider id, or a kind without an adapter, is an internal error (provider
//! resolution failure before streaming: 500).

use std::collections::BTreeMap;

use uuid::Uuid;

use super::{LlmAdapters, kind_name};
use crate::config::{ProviderEntry, TenantOverride};
use crate::domain::error::DomainError;
use crate::domain::llm::{LlmProviderResolver, ProviderTarget};

/// Resolver over a copy of the provider entries.
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    providers: BTreeMap<String, ProviderEntry>,
}

impl ProviderResolver {
    /// Resolver over the configured provider entries.
    #[must_use]
    pub fn new(providers: &BTreeMap<String, ProviderEntry>) -> Self {
        Self {
            providers: providers.clone(),
        }
    }
}

/// Tenant override of `entry` for `tenant_id`; keys are tenant UUID strings
/// (compared as UUIDs, so case and hyphenation do not matter).
pub(crate) fn tenant_override(entry: &ProviderEntry, tenant_id: Uuid) -> Option<&TenantOverride> {
    entry
        .tenant_overrides
        .iter()
        .find(|(key, _)| Uuid::parse_str(key.trim()).is_ok_and(|id| id == tenant_id))
        .map(|(_, entry)| entry)
}

/// Upstream alias of `entry` for `tenant_id`: the tenant override's alias (or
/// host) when one applies, else the entry's `upstream_alias`, else its host.
pub(crate) fn tenant_alias(entry: &ProviderEntry, tenant_id: Uuid) -> &str {
    tenant_override(entry, tenant_id)
        .and_then(|o| o.upstream_alias.as_deref().or(o.host.as_deref()))
        .or(entry.upstream_alias.as_deref())
        .unwrap_or(&entry.host)
}

impl LlmProviderResolver for ProviderResolver {
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError> {
        let entry = self.providers.get(provider_id).ok_or_else(|| {
            DomainError::internal(format!(
                "model provider '{provider_id}' is not configured in mini-chat providers"
            ))
        })?;
        if !LlmAdapters::supports(entry.kind) {
            return Err(DomainError::internal(format!(
                "model provider '{provider_id}' uses kind '{}', which has no adapter",
                kind_name(entry.kind)
            )));
        }
        let override_alias = tenant_override(entry, tenant_id)
            .and_then(|o| o.upstream_alias.as_deref().or(o.host.as_deref()));
        let alias = override_alias
            .or(entry.upstream_alias.as_deref())
            .unwrap_or(&entry.host);
        Ok(ProviderTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: alias.to_owned(),
            api_path: entry.api_path.clone(),
            tenant_id,
            tenant_override: override_alias.is_some(),
        })
    }
}

#[cfg(test)]
#[path = "resolver_tests.rs"]
mod tests;
