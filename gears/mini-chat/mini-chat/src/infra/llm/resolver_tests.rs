use std::collections::BTreeMap;

use uuid::Uuid;

use super::ProviderResolver;
use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind};
use crate::domain::error::DomainError;
use crate::domain::llm::LlmProviderResolver;

const TENANT_A: &str = "a0000000-0000-4000-8000-00000000000a";
const TENANT_B: &str = "b0000000-0000-4000-8000-00000000000b";
const TENANT_C: &str = "c0000000-0000-4000-8000-00000000000c";

/// Providers as prepared by gear init (aliases filled).
fn providers() -> BTreeMap<String, ProviderEntry> {
    let mut config: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "storage_kind": "openai",
                "host": "api.openai.com",
            },
            "azure": {
                "kind": "openai_responses",
                "storage_kind": "azure",
                "api_version": "2025-04-01-preview",
                "host": "shared.openai.azure.com",
                "upstream_alias": "azure-shared",
                "api_path": "/openai/v1/responses?api-version=preview",
                "tenant_overrides": {
                    TENANT_A: { "host": "tenant-a.openai.azure.com" },
                    "B0000000-0000-4000-8000-00000000000B": { "upstream_alias": "azure-b" },
                },
            },
            "claude": {
                "kind": "anthropic_messages",
                "storage_kind": "openai",
                "host": "api.anthropic.com",
                "api_path": "/v1/messages",
            },
        }
    }))
    .unwrap();
    config.fill_upstream_aliases();
    config.providers
}

fn tenant(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap()
}

#[test]
fn entry_alias_defaults_to_the_host() {
    let resolver = ProviderResolver::new(&providers());
    let target = resolver.resolve("openai", tenant(TENANT_A)).unwrap();
    assert_eq!(target.provider_id, "openai");
    assert_eq!(target.kind, ProviderKind::OpenaiResponses);
    assert_eq!(target.alias, "api.openai.com");
    assert_eq!(target.api_path, "/v1/responses");
    assert_eq!(target.tenant_id, tenant(TENANT_A));
    assert!(!target.tenant_override);
}

#[test]
fn configured_alias_is_used_without_override() {
    let resolver = ProviderResolver::new(&providers());
    let target = resolver.resolve("azure", tenant(TENANT_C)).unwrap();
    assert_eq!(target.alias, "azure-shared");
    assert_eq!(target.api_path, "/openai/v1/responses?api-version=preview");
    assert!(!target.tenant_override);
}

#[test]
fn tenant_override_alias_wins() {
    let resolver = ProviderResolver::new(&providers());

    let a = resolver.resolve("azure", tenant(TENANT_A)).unwrap();
    assert_eq!(
        a.alias, "tenant-a.openai.azure.com",
        "override host fills its alias"
    );
    assert!(a.tenant_override);

    // Keys are compared as UUIDs (case-insensitive).
    let b = resolver.resolve("azure", tenant(TENANT_B)).unwrap();
    assert_eq!(b.alias, "azure-b");
    assert!(b.tenant_override);
}

#[test]
fn unknown_provider_is_internal() {
    let resolver = ProviderResolver::new(&providers());
    let err = resolver.resolve("missing", tenant(TENANT_A)).unwrap_err();
    assert!(
        matches!(err, DomainError::Internal(ref msg) if msg.contains("missing")),
        "{err}"
    );
}

#[test]
fn every_adapter_kind_resolves() {
    let resolver = ProviderResolver::new(&providers());
    let target = resolver.resolve("claude", tenant(TENANT_A)).unwrap();
    assert_eq!(target.kind, ProviderKind::AnthropicMessages);
    assert_eq!(target.alias, "api.anthropic.com");
    assert_eq!(target.api_path, "/v1/messages");
    for kind in [
        ProviderKind::OpenaiResponses,
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
        ProviderKind::AnthropicMessages,
    ] {
        assert!(super::LlmAdapters::supports(kind), "{kind:?}");
    }
}
