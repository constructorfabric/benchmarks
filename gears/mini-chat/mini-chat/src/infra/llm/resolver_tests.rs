#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderKind, StorageKind};

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";

fn cfg() -> MiniChatConfig {
    let mut cfg: MiniChatConfig = serde_json::from_value(json!({
        "client_credentials": {"client_id": "id", "client_secret": "secret"},
        "providers": {
            "openai": {
                "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai",
                "tenant_overrides": {TENANT: {"host": "eu.openai.example.com"}}
            },
            "azure": {
                "kind": "openai_responses", "host": "my.openai.azure.com",
                "api_path": "/openai/v1/responses?api-version=preview",
                "storage_kind": "azure", "api_version": "2025-03-01-preview",
                "storage_backend": "azure-blob",
                "tenant_overrides": {TENANT: {"upstream_alias": "tenant-azure"}}
            },
            "claude": {
                "kind": "anthropic_messages", "host": "api.anthropic.com",
                "api_path": "/v1/messages", "rag_provider": "azure"
            }
        }
    }))
    .unwrap();
    cfg.fill_upstream_aliases();
    cfg.validate().unwrap();
    cfg
}

fn tenant() -> Uuid {
    Uuid::parse_str(TENANT).unwrap()
}

#[test]
fn resolver_uses_tenant_override_alias() {
    let r = ProviderResolver::new(&cfg());
    let other = r.resolve("openai", Uuid::new_v4()).unwrap();
    assert_eq!(other.alias, "api.openai.com");
    assert_eq!(other.kind, ProviderKind::OpenaiResponses);
    assert_eq!(other.api_path, "/v1/responses");

    let own = r.resolve("openai", tenant()).unwrap();
    assert_eq!(own.provider_id, "openai");
    assert_eq!(own.alias, "eu.openai.example.com");
    // The tenant override also applies to the storage alias of the same entry.
    assert_eq!(own.storage.unwrap().alias, "eu.openai.example.com");

    let az = r.resolve("azure", tenant()).unwrap();
    assert_eq!(az.alias, "tenant-azure");
    assert_eq!(az.api_path, "/openai/v1/responses?api-version=preview");
}

#[test]
fn unknown_provider_is_provider_resolution_error() {
    let r = ProviderResolver::new(&cfg());
    assert!(matches!(
        r.resolve("nope", tenant()),
        Err(DomainError::ProviderResolution(_))
    ));
    assert!(matches!(
        r.resolve_storage("nope", tenant()),
        Err(DomainError::ProviderResolution(_))
    ));
}

#[test]
fn rag_provider_redirects_storage() {
    let r = ProviderResolver::new(&cfg());
    let claude = r.resolve("claude", Uuid::new_v4()).unwrap();
    assert_eq!(claude.kind, ProviderKind::AnthropicMessages);
    assert_eq!(claude.alias, "api.anthropic.com");
    let storage = claude.storage.unwrap();
    assert_eq!(
        storage,
        ResolvedStorage {
            provider_id: "azure".to_owned(),
            kind: StorageKind::Azure,
            alias: "my.openai.azure.com".to_owned(),
            api_version: Some("2025-03-01-preview".to_owned()),
            backend_label: "azure-blob".to_owned(),
        }
    );
    // Storage of the redirect target honours the target's tenant override.
    assert_eq!(
        r.resolve_storage("claude", tenant()).unwrap().alias,
        "tenant-azure"
    );

    let own = r.resolve_storage("openai", Uuid::new_v4()).unwrap();
    assert_eq!(own.provider_id, "openai");
    assert_eq!(own.kind, StorageKind::Openai);
    assert_eq!(own.api_version, None);
    assert_eq!(own.backend_label, "openai");
}

#[test]
fn backend_label_maps_back() {
    let r = ProviderResolver::new(&cfg());
    let az = r
        .storage_for_backend_label("azure-blob", Uuid::new_v4())
        .unwrap();
    assert_eq!(az.provider_id, "azure");
    assert_eq!(az.alias, "my.openai.azure.com");
    assert_eq!(az.kind, StorageKind::Azure);
    let oa = r
        .storage_for_backend_label("openai", Uuid::new_v4())
        .unwrap();
    assert_eq!(oa.provider_id, "openai");
    assert_eq!(oa.alias, "api.openai.com");
    // `claude` has no storage of its own, so its id is not a backend label.
    assert!(matches!(
        r.storage_for_backend_label("claude", Uuid::new_v4()),
        Err(DomainError::ProviderResolution(_))
    ));
    assert!(
        r.storage_for_backend_label("azure", Uuid::new_v4())
            .is_err()
    );
}

#[test]
fn backend_label_honours_tenant_override_alias() {
    let r = ProviderResolver::new(&cfg());
    // Same alias as creation (`resolve_storage`), for every override kind.
    let az = r.storage_for_backend_label("azure-blob", tenant()).unwrap();
    assert_eq!(az.alias, "tenant-azure");
    assert_eq!(
        az.alias,
        r.resolve_storage("azure", tenant()).unwrap().alias
    );
    assert_eq!(
        az.alias,
        r.resolve_storage("claude", tenant()).unwrap().alias
    );
    let oa = r.storage_for_backend_label("openai", tenant()).unwrap();
    assert_eq!(oa.alias, "eu.openai.example.com");
    assert_eq!(
        oa.alias,
        r.resolve_storage("openai", tenant()).unwrap().alias
    );
    // Tenants without an override keep the base alias.
    let other = Uuid::new_v4();
    assert_eq!(
        r.storage_for_backend_label("azure-blob", other)
            .unwrap()
            .alias,
        "my.openai.azure.com"
    );
    assert_eq!(
        r.storage_for_backend_label("openai", other).unwrap().alias,
        "api.openai.com"
    );
}

#[test]
fn knowledge_target_uses_entry_alias_and_api_version() {
    let r = ProviderResolver::new(&cfg());
    let t = r.knowledge_target("azure", "vs_kb", tenant()).unwrap();
    assert_eq!(
        t,
        crate::infra::llm::knowledge::KnowledgeTarget {
            alias: "tenant-azure".to_owned(),
            api_version: "2025-03-01-preview".to_owned(),
            vector_store_id: "vs_kb".to_owned(),
        }
    );
    let other = r
        .knowledge_target("azure", "vs_kb", Uuid::new_v4())
        .unwrap();
    assert_eq!(other.alias, "my.openai.azure.com");
}

#[test]
fn knowledge_target_requires_api_version_and_supported_kind() {
    let mut c = cfg();
    c.providers.get_mut("azure").unwrap().api_version = Some("  ".to_owned());
    c.providers.get_mut("claude").unwrap().api_version = Some("v1".to_owned());
    c.providers.get_mut("openai").unwrap().kind = ProviderKind::VllmResponses;
    c.providers.get_mut("openai").unwrap().api_version = Some("v1".to_owned());
    let r = ProviderResolver::new(&c);
    assert!(matches!(
        r.knowledge_target("azure", "vs", tenant()),
        Err(DomainError::ProviderResolution(_))
    ));
    assert!(r.knowledge_target("openai", "vs", tenant()).is_err());
    assert!(r.knowledge_target("missing", "vs", tenant()).is_err());
    // anthropic_messages entries are accepted.
    assert_eq!(
        r.knowledge_target("claude", "vs", tenant()).unwrap().alias,
        "api.anthropic.com"
    );
}
