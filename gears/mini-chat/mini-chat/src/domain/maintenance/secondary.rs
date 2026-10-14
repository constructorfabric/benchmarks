//! The deterministic alias of the Anthropic secondary file delete of chat cleanup
//! (`cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file`, steps `inst-sf-model`,
//! `inst-sf-provider` and `inst-sf-fallback`).

use mini_chat_sdk::PolicySnapshot;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::provider_registry::ProviderRegistry;

/// Whether the registry holds an `anthropic_messages` provider (an entry or a tenant override of
/// one): only then the chat-cleanup handler is built with the Anthropic Files client.
#[must_use]
pub fn has_anthropic_provider(providers: &ProviderRegistry) -> bool {
    providers.entries().any(|entry| {
        std::iter::once(&entry.base)
            .chain(entry.overrides.values())
            .any(|provider| provider.kind == ProviderKind::AnthropicMessages)
    })
}

/// The upstream alias of the Anthropic Files delete of a chat of `tenant_id` whose model is
/// `chat_model`: the provider of the model's catalog entry (without the enabled filter) resolved
/// for the tenant, accepted only for the kind `anthropic_messages`; when `snapshot` is `None`
/// (unreadable), the model left the catalog, or the provider cannot be resolved or is not
/// Anthropic, the tenant's single `anthropic_messages` provider; `None` when there is none or
/// several (no guess between several accounts).
#[must_use]
pub fn resolve_secondary_alias(
    snapshot: Option<&PolicySnapshot>,
    providers: &ProviderRegistry,
    tenant_id: Uuid,
    chat_model: &str,
) -> Option<String> {
    let by_model = snapshot
        .and_then(|snapshot| {
            snapshot
                .model_catalog
                .iter()
                .find(|entry| entry.id == chat_model)
        })
        .and_then(|entry| providers.resolve(&entry.provider_id, tenant_id).ok())
        .filter(|provider| provider.kind == ProviderKind::AnthropicMessages)
        .map(|provider| provider.upstream_alias.clone());
    if by_model.is_some() {
        return by_model;
    }
    let mut anthropic = providers
        .entries()
        .filter_map(|entry| providers.resolve(&entry.base.provider_id, tenant_id).ok())
        .filter(|provider| provider.kind == ProviderKind::AnthropicMessages);
    match (anthropic.next(), anthropic.next()) {
        (Some(only), None) => Some(only.upstream_alias.clone()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "secondary_tests.rs"]
mod secondary_tests;
