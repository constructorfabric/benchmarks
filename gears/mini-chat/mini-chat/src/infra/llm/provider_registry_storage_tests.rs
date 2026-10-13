#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the provider entries as storage targets
//! (`cpt-cf-mini-chat-algo-attachments-resolve-upload-context`, steps 3 to 5, and
//! the backend lookup of `cpt-cf-mini-chat-algo-attachments-handle-cleanup`).

use serde_json::{Value, json};
use uuid::Uuid;

use super::ProviderResolver;
use crate::config::StorageKind;
use crate::domain::error::DomainError;
use crate::domain::service::attachments::ports::StorageDirectory;
use crate::test_doubles::{provider_json, registry, registry_ordered};

const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0xA1);
const PLAIN_TENANT: Uuid = Uuid::from_u128(0xB2);

fn entries() -> Vec<(&'static str, Value)> {
    let mut openai = provider_json("api.openai.test", "openai");
    openai["tenant_overrides"] = json!({
        OVERRIDE_TENANT.to_string(): { "host": "tenant-a.openai.test" },
    });
    let azure = provider_json("az.test", "azure");
    let mut eu = provider_json("eu.azure.test", "azure");
    eu["storage_backend"] = json!("azure-eu");
    let mut anthropic = provider_json("api.anthropic.test", "openai");
    anthropic["kind"] = json!("anthropic_messages");
    anthropic["api_path"] = json!("/v1/messages");
    anthropic["rag_provider"] = json!("openai");
    anthropic["tenant_overrides"] = json!({
        OVERRIDE_TENANT.to_string(): { "host": "tenant-a.anthropic.test" },
    });
    vec![
        ("openai", openai),
        ("azure", azure),
        ("eu", eu),
        ("anthropic", anthropic),
    ]
}

fn resolver() -> ProviderResolver {
    ProviderResolver::new(&registry_ordered(&entries()))
}

#[test]
fn an_openai_provider_is_its_own_storage_without_a_secondary_copy() {
    let targets = resolver().upload_targets("openai", PLAIN_TENANT).unwrap();

    assert_eq!(targets.storage.provider_id, "openai");
    assert_eq!(targets.storage.kind, StorageKind::Openai);
    assert_eq!(targets.storage.backend, "openai");
    assert_eq!(targets.storage.alias, "api.openai.test");
    assert_eq!(targets.storage.api_version, None);
    assert_eq!(targets.secondary_alias, None);
}

#[test]
fn an_azure_provider_carries_its_api_version() {
    let targets = resolver().upload_targets("azure", PLAIN_TENANT).unwrap();

    assert_eq!(targets.storage.kind, StorageKind::Azure);
    assert_eq!(targets.storage.alias, "az.test");
    assert_eq!(
        targets.storage.api_version.as_deref(),
        Some("2025-01-01-preview")
    );
}

#[test]
fn the_backend_label_is_the_configured_one_or_the_provider_id() {
    let resolver = resolver();

    assert_eq!(
        resolver
            .upload_targets("azure", PLAIN_TENANT)
            .unwrap()
            .storage
            .backend,
        "azure"
    );
    assert_eq!(
        resolver
            .upload_targets("eu", PLAIN_TENANT)
            .unwrap()
            .storage
            .backend,
        "azure-eu"
    );
}

#[test]
fn an_anthropic_chat_stores_in_its_rag_provider_and_copies_images_to_anthropic() {
    let targets = resolver()
        .upload_targets("anthropic", PLAIN_TENANT)
        .unwrap();

    assert_eq!(targets.storage.provider_id, "openai");
    assert_eq!(targets.storage.alias, "api.openai.test");
    assert_eq!(targets.storage.backend, "openai");
    assert_eq!(
        targets.secondary_alias.as_deref(),
        Some("api.anthropic.test")
    );
}

#[test]
fn the_aliases_are_those_of_the_tenant_override() {
    let targets = resolver()
        .upload_targets("anthropic", OVERRIDE_TENANT)
        .unwrap();

    assert_eq!(targets.storage.alias, "tenant-a.openai.test");
    assert_eq!(
        targets.secondary_alias.as_deref(),
        Some("tenant-a.anthropic.test")
    );
    let own = resolver()
        .upload_targets("openai", OVERRIDE_TENANT)
        .unwrap();
    assert_eq!(own.storage.alias, "tenant-a.openai.test");
}

#[test]
fn an_unconfigured_provider_is_a_provider_resolution_error_naming_it() {
    assert!(
        resolver().upload_targets("openai", PLAIN_TENANT).is_ok(),
        "a configured one resolves"
    );
    let err = resolver()
        .upload_targets("mistral", PLAIN_TENANT)
        .expect_err("not configured");

    match err {
        DomainError::ProviderResolution(message) => {
            assert!(message.contains("mistral"), "{message}");
        }
        other => panic!("expected ProviderResolution, got {other:?}"),
    }
}

#[test]
fn the_backend_lookup_finds_the_entry_with_that_label() {
    let resolver = resolver();

    let azure = resolver.storage_for_backend("azure", PLAIN_TENANT).unwrap();
    assert_eq!(azure.provider_id, "azure");
    assert_eq!(azure.kind, StorageKind::Azure);
    assert_eq!(azure.alias, "az.test");
    assert_eq!(azure.api_version.as_deref(), Some("2025-01-01-preview"));

    let eu = resolver
        .storage_for_backend("azure-eu", PLAIN_TENANT)
        .unwrap();
    assert_eq!(eu.provider_id, "eu");
    assert_eq!(eu.alias, "eu.azure.test");
}

#[test]
fn the_backend_lookup_uses_the_alias_of_the_tenant() {
    let target = resolver()
        .storage_for_backend("openai", OVERRIDE_TENANT)
        .unwrap();

    assert_eq!(target.alias, "tenant-a.openai.test");
}

#[test]
fn an_unknown_backend_label_is_none() {
    assert!(
        resolver()
            .storage_for_backend("openai", PLAIN_TENANT)
            .is_some()
    );
    assert!(
        resolver()
            .storage_for_backend("gcs", PLAIN_TENANT)
            .is_none()
    );
    assert!(resolver().storage_for_backend("", PLAIN_TENANT).is_none());
}

#[test]
fn the_first_entry_in_configuration_order_wins_a_shared_label() {
    let mut first = provider_json("first.test", "openai");
    first["storage_backend"] = json!("shared");
    let mut second = provider_json("second.test", "openai");
    second["storage_backend"] = json!("shared");
    let resolver = ProviderResolver::new(&registry_ordered(&[("zeta", first), ("alpha", second)]));

    let target = resolver
        .storage_for_backend("shared", PLAIN_TENANT)
        .unwrap();

    assert_eq!(
        target.provider_id, "zeta",
        "configuration order, not alphabetical"
    );
    assert_eq!(target.alias, "first.test");
    let _ = registry;
}
