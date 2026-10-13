//! The provider resolver: maps a catalog model's `provider_id` and the tenant to a provider.

use std::collections::HashMap;
use std::sync::Arc;

use oagw_sdk::ServiceGatewayClientV1;
use uuid::Uuid;

use super::adapter::adapter_for;
use super::alias::upstream_alias_for_tenant;
use super::provider::OagwProvider;
use crate::config::ProviderEntry;
use crate::domain::error::DomainError;
use crate::domain::llm::{LlmProvider, LlmProviderResolver};

/// Resolves providers from the `providers` section of the gear configuration.
pub struct ConfiguredProviders {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    entries: HashMap<String, ProviderEntry>,
}

impl ConfiguredProviders {
    /// Creates the resolver over the provider entries.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        entries: HashMap<String, ProviderEntry>,
    ) -> Self {
        Self { gateway, entries }
    }
}

impl LlmProviderResolver for ConfiguredProviders {
    fn resolve(
        &self,
        tenant_id: Uuid,
        provider_id: &str,
    ) -> Result<Arc<dyn LlmProvider>, DomainError> {
        let entry =
            self.entries
                .get(provider_id)
                .ok_or_else(|| DomainError::PolicyUnavailable {
                    diagnostic: format!("provider `{provider_id}` is not configured"),
                })?;
        let tenant_override = entry.tenant_overrides.get(&tenant_id.to_string());
        let alias = upstream_alias_for_tenant(entry, tenant_override);
        Ok(Arc::new(OagwProvider::new(
            provider_id.to_owned(),
            alias,
            entry.api_path.clone(),
            adapter_for(entry.kind),
            Arc::clone(&self.gateway),
        )))
    }
}
