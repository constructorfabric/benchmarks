#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use uuid::Uuid;

use super::ProviderResolver;
use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind, TenantOverride};
use crate::domain::error::DomainError;

const TENANT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
const OTHER: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);

fn azure_entry() -> ProviderEntry {
    let mut e = MiniChatConfig::default().providers["openai"].clone();
    e.host = "my.openai.azure.com".to_owned();
    e.api_path = "/openai/v1/responses?api-version=preview".to_owned();
    e.storage_kind = StorageKind::Azure;
    e.api_version = Some("2025-04-01-preview".to_owned());
    e
}

fn config(f: impl FnOnce(&mut HashMap<String, ProviderEntry>)) -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    "test-client".clone_into(&mut cfg.client_credentials.client_id);
    cfg.client_credentials.client_secret = "test-secret".to_owned().into();
    f(&mut cfg.providers);
    cfg.apply_defaults();
    cfg.validate().unwrap();
    cfg
}

#[test]
fn default_openai_resolves_to_host_alias() {
    let r = ProviderResolver::new(&config(|_| {}));
    let p = r.resolve("openai", TENANT).unwrap();
    assert_eq!(p.provider_id, "openai");
    assert_eq!(p.kind, ProviderKind::OpenaiResponses);
    assert_eq!(p.alias, "api.openai.com");
    assert_eq!(p.api_path, "/v1/responses");
    assert_eq!(p.storage_kind, Some(StorageKind::Openai));
    assert_eq!(p.storage_backend, "openai");
    assert_eq!(p.api_version, None);
    assert_eq!(p.chat_uri("gpt-5.2"), "/api.openai.com/v1/responses");
}

#[test]
fn tenant_override_changes_alias() {
    let cfg = config(|providers| {
        let e = providers.get_mut("openai").unwrap();
        e.tenant_overrides.insert(
            TENANT,
            TenantOverride {
                host: Some("tenant-proxy.example.com".to_owned()),
                ..TenantOverride::default()
            },
        );
        e.tenant_overrides.insert(
            OTHER,
            TenantOverride {
                upstream_alias: Some("other-alias".to_owned()),
                ..TenantOverride::default()
            },
        );
    });
    let r = ProviderResolver::new(&cfg);
    assert_eq!(
        r.resolve("openai", TENANT).unwrap().alias,
        "tenant-proxy.example.com"
    );
    assert_eq!(r.resolve("openai", OTHER).unwrap().alias, "other-alias");
    assert_eq!(
        r.resolve("openai", Uuid::nil()).unwrap().alias,
        "api.openai.com"
    );
}

#[test]
fn model_placeholder_replaced() {
    let cfg = config(|providers| {
        let mut e = providers["openai"].clone();
        e.kind = ProviderKind::OpenaiChatCompletions;
        e.host = "llm.internal".to_owned();
        e.upstream_alias = Some("llm".to_owned());
        e.api_path = "/v1/deployments/{model}/chat?api-version=1".to_owned();
        providers.insert("dep".to_owned(), e);
    });
    let p = ProviderResolver::new(&cfg).resolve("dep", TENANT).unwrap();
    assert_eq!(p.kind, ProviderKind::OpenaiChatCompletions);
    assert_eq!(
        p.chat_uri("gpt-4o"),
        "/llm/v1/deployments/gpt-4o/chat?api-version=1"
    );
}

#[test]
fn rag_provider_used_for_storage() {
    let cfg = config(|providers| {
        providers.insert("azure".to_owned(), azure_entry());
        let mut chat = providers["openai"].clone();
        chat.host = "chat.example.com".to_owned();
        chat.rag_provider = Some("azure".to_owned());
        providers.insert("chat".to_owned(), chat);
    });
    let r = ProviderResolver::new(&cfg);

    let rag = r.resolve_rag("chat", TENANT).unwrap();
    assert_eq!(rag.provider_id, "azure");
    assert_eq!(rag.alias, "my.openai.azure.com");
    assert_eq!(rag.storage_kind, Some(StorageKind::Azure));
    assert_eq!(rag.storage_backend, "azure");
    assert_eq!(rag.api_version.as_deref(), Some("2025-04-01-preview"));

    // Without `rag_provider` the entry serves its own storage.
    assert_eq!(
        r.resolve_rag("openai", TENANT).unwrap().provider_id,
        "openai"
    );
}

#[test]
fn storage_label_maps_back_to_its_provider_entry() {
    let cfg = config(|providers| {
        // Two entries share one storage label; a third has the label as id only.
        let mut a = providers["openai"].clone();
        a.host = "a.example.com".to_owned();
        a.storage_backend = Some("shared".to_owned());
        providers.insert("b-provider".to_owned(), a.clone());
        a.host = "first.example.com".to_owned();
        providers.insert("a-provider".to_owned(), a);
        providers
            .get_mut("openai")
            .unwrap()
            .tenant_overrides
            .insert(
                TENANT,
                TenantOverride {
                    host: Some("tenant-proxy.example.com".to_owned()),
                    ..TenantOverride::default()
                },
            );
    });
    let r = ProviderResolver::new(&cfg);

    // Label = provider id (default `storage_backend`).
    let p = r.resolve_storage("openai", OTHER).unwrap();
    assert_eq!(p.provider_id, "openai");
    assert_eq!(p.alias, "api.openai.com");
    // Tenant overrides apply.
    assert_eq!(
        r.resolve_storage("openai", TENANT).unwrap().alias,
        "tenant-proxy.example.com"
    );
    // A custom label resolves to the entry that declares it (lowest id first).
    let p = r.resolve_storage("shared", TENANT).unwrap();
    assert_eq!(p.provider_id, "a-provider");
    assert_eq!(p.alias, "first.example.com");
    assert_eq!(p.storage_backend, "shared");
    // Unknown label.
    assert!(matches!(
        r.resolve_storage("nope", TENANT),
        Err(DomainError::Internal(_))
    ));
}

#[test]
fn unknown_provider_is_internal() {
    let r = ProviderResolver::new(&config(|_| {}));
    assert!(matches!(
        r.resolve("missing", TENANT),
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        r.resolve_rag("missing", TENANT),
        Err(DomainError::Internal(_))
    ));
}

#[test]
fn alias_override_replaces_entry_and_tenant_alias() {
    let cfg = config(|providers| {
        providers.get_mut("openai").unwrap().tenant_overrides.insert(
            TENANT,
            TenantOverride {
                host: Some("tenant-proxy.example.com".to_owned()),
                ..TenantOverride::default()
            },
        );
    });
    let r = ProviderResolver::new(&cfg);

    // Entry alias: every tenant without its own override.
    r.set_alias_override("openai", None, "api.openai.com:8443");
    assert_eq!(
        r.resolve("openai", OTHER).unwrap().alias,
        "api.openai.com:8443"
    );
    assert_eq!(
        r.resolve("openai", OTHER).unwrap().chat_uri("m"),
        "/api.openai.com:8443/v1/responses"
    );
    // The tenant override keeps its own alias until it is overridden too.
    assert_eq!(
        r.resolve("openai", TENANT).unwrap().alias,
        "tenant-proxy.example.com"
    );
    r.set_alias_override("openai", Some(TENANT), "tenant-proxy.example.com:9000");
    assert_eq!(
        r.resolve("openai", TENANT).unwrap().alias,
        "tenant-proxy.example.com:9000"
    );
    assert_eq!(
        r.resolve("openai", OTHER).unwrap().alias,
        "api.openai.com:8443"
    );
}
