use std::collections::BTreeMap;

use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderKind, StorageKind};

const TENANT_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";

fn providers() -> BTreeMap<String, ProviderEntry> {
    let mut cfg: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "client_credentials": {"client_id": "c", "client_secret": "s"},
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "host": "api.openai.com",
                "storage_kind": "openai",
                "tenant_overrides": {
                    TENANT_A: {"host": "tenant-a.openai.example"}
                }
            },
            "azure": {
                "kind": "openai_responses",
                "host": "10.1.0.1",
                "upstream_alias": "azure-main",
                "api_path": "/openai/v1/responses?api-version=preview",
                "storage_kind": "azure",
                "api_version": "2025-03-01-preview",
                "storage_backend": "azure-store",
                "tenant_overrides": {
                    TENANT_A: {"upstream_alias": "azure-tenant-a"}
                }
            },
            "claude": {
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "api_path": "/v1/messages",
                "storage_kind": "openai",
                "rag_provider": "azure"
            }
        }
    }))
    .unwrap();
    cfg.validate().unwrap();
    cfg.providers
}

fn tenant_a() -> Uuid {
    Uuid::parse_str(TENANT_A).unwrap()
}

#[test]
fn resolves_entry_alias_kind_and_path() {
    let r = ProviderResolver::new(&providers());
    let t = r.resolve("openai", Uuid::from_u128(9)).unwrap();
    assert_eq!(
        t,
        ProviderTarget {
            provider_id: "openai".into(),
            kind: ProviderKind::OpenaiResponses,
            alias: "api.openai.com".into(),
            api_path: "/v1/responses".into(),
        }
    );
    let t = r.resolve("azure", Uuid::from_u128(9)).unwrap();
    assert_eq!(t.alias, "azure-main");
    assert_eq!(t.api_path, "/openai/v1/responses?api-version=preview");
}

#[test]
fn tenant_override_selects_its_alias() {
    let r = ProviderResolver::new(&providers());
    assert_eq!(
        r.resolve("openai", tenant_a()).unwrap().alias,
        "tenant-a.openai.example"
    );
    assert_eq!(
        r.resolve("azure", tenant_a()).unwrap().alias,
        "azure-tenant-a"
    );
}

#[test]
fn unknown_provider_is_resolution_error() {
    let r = ProviderResolver::new(&providers());
    assert!(matches!(
        r.resolve("nope", tenant_a()),
        Err(DomainError::ProviderResolution(_))
    ));
    assert!(matches!(
        r.resolve_storage("nope", tenant_a()),
        Err(DomainError::ProviderResolution(_))
    ));
}

#[test]
fn storage_follows_rag_provider_and_its_override() {
    let r = ProviderResolver::new(&providers());
    let s = r.resolve_storage("claude", Uuid::from_u128(9)).unwrap();
    assert_eq!(
        s,
        StorageTarget {
            provider_id: "azure".into(),
            storage_kind: StorageKind::Azure,
            alias: "azure-main".into(),
            api_version: Some("2025-03-01-preview".into()),
            storage_backend: "azure-store".into(),
        }
    );
    assert_eq!(
        r.resolve_storage("claude", tenant_a()).unwrap().alias,
        "azure-tenant-a"
    );
}

#[test]
fn storage_of_entry_without_rag_provider_is_itself() {
    let r = ProviderResolver::new(&providers());
    let s = r.resolve_storage("openai", tenant_a()).unwrap();
    assert_eq!(s.provider_id, "openai");
    assert_eq!(s.storage_kind, StorageKind::Openai);
    assert_eq!(s.alias, "tenant-a.openai.example");
    assert_eq!(s.api_version, None);
    assert_eq!(s.storage_backend, "openai");
}

#[test]
fn chat_uri_replaces_model_placeholder() {
    let t = ProviderTarget {
        provider_id: "p".into(),
        kind: ProviderKind::OpenaiChatCompletions,
        alias: "host.example".into(),
        api_path: "/openai/deployments/{model}/chat/completions?api-version=1".into(),
    };
    assert_eq!(
        t.chat_uri("gpt-4o"),
        "/host.example/openai/deployments/gpt-4o/chat/completions?api-version=1"
    );
}

#[test]
fn resolves_storage_by_backend_label() {
    let r = ProviderResolver::new(&providers());
    // Label from `storage_backend`, with the tenant's override alias.
    assert_eq!(
        r.resolve_storage_backend("azure-store", tenant_a())
            .unwrap(),
        r.resolve_storage("claude", tenant_a()).unwrap()
    );
    // Label defaulting to the provider id.
    let t = r
        .resolve_storage_backend("openai", Uuid::from_u128(9))
        .unwrap();
    assert_eq!(t, r.resolve_storage("openai", Uuid::from_u128(9)).unwrap());
    // A provider id whose label differs does not match.
    assert!(matches!(
        r.resolve_storage_backend("azure", Uuid::from_u128(9)),
        Err(DomainError::ProviderResolution(_))
    ));
}
