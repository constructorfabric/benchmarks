#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`.

use serde_json::{Value, json};
use uuid::Uuid;

use super::ProviderResolver;
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::test_doubles::{provider_json, registry};

const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0xA1);
const PLAIN_TENANT: Uuid = Uuid::from_u128(0xB2);
const ALIAS_OVERRIDE_TENANT: Uuid = Uuid::from_u128(0xC3);

fn providers() -> Value {
    let mut openai = provider_json("api.openai.com", "openai");
    openai["tenant_overrides"] = json!({
        OVERRIDE_TENANT.to_string(): { "host": "tenant-a.openai.example.com" },
        ALIAS_OVERRIDE_TENANT.to_string(): {
            "host": "tenant-c.openai.example.com",
            "upstream_alias": "tenant-c-alias"
        },
    });

    let mut azure = provider_json("res.openai.azure.com", "azure");
    azure["upstream_alias"] = json!("azure-main");
    azure["api_path"] =
        json!("/openai/deployments/{model}/chat/completions?api-version=2025-01-01");
    azure["kind"] = json!("openai_chat_completions");
    azure["rag_provider"] = json!("openai");

    json!({ "openai": openai, "azure": azure })
}

fn resolver() -> ProviderResolver {
    ProviderResolver::new(&registry(&providers()))
}

#[test]
fn a_missing_provider_is_a_provider_resolution_error_naming_it() {
    let err = resolver()
        .resolve("anthropic", PLAIN_TENANT)
        .expect_err("not configured");

    match err {
        DomainError::ProviderResolution(message) => {
            assert!(message.contains("anthropic"), "{message}");
        }
        other => panic!("expected ProviderResolution (500 internal), got {other:?}"),
    }
}

#[test]
fn the_entry_alias_defaults_to_the_host_for_a_tenant_without_override() {
    let resolution = resolver()
        .resolve("openai", PLAIN_TENANT)
        .expect("resolves");

    assert_eq!(resolution.alias, "api.openai.com");
    assert_eq!(resolution.kind, ProviderKind::OpenaiResponses);
    assert_eq!(resolution.api_path, "/v1/responses");
}

#[test]
fn a_configured_hostname_alias_is_replaced_by_the_derived_alias_for_every_tenant() {
    let resolution = resolver()
        .resolve("azure", OVERRIDE_TENANT)
        .expect("resolves");

    assert_eq!(resolution.alias, "res.openai.azure.com");
    assert_eq!(resolution.kind, ProviderKind::OpenaiChatCompletions);
}

#[test]
fn the_override_alias_is_used_for_the_override_tenant() {
    let resolution = resolver()
        .resolve("openai", OVERRIDE_TENANT)
        .expect("resolves");

    assert_eq!(resolution.alias, "tenant-a.openai.example.com");
}

#[test]
fn a_hostname_override_alias_is_replaced_by_the_derived_alias() {
    let resolution = resolver()
        .resolve("openai", ALIAS_OVERRIDE_TENANT)
        .expect("resolves");

    assert_eq!(resolution.alias, "tenant-c.openai.example.com");
}

#[test]
fn an_ip_literal_entry_keeps_its_normalized_configured_alias() {
    let mut entry = provider_json("10.1.2.3", "openai");
    entry["upstream_alias"] = json!("Internal-LLM.");
    let resolver = ProviderResolver::new(&registry(&json!({ "ip": entry })));

    let resolution = resolver.resolve("ip", PLAIN_TENANT).expect("resolves");

    assert_eq!(resolution.alias, "internal-llm");
}

#[test]
fn a_non_standard_port_is_part_of_the_resolved_alias() {
    let mut entry = provider_json("localhost", "openai");
    entry["port"] = json!(18418);
    entry["use_http"] = json!(true);
    let resolver = ProviderResolver::new(&registry(&json!({ "local": entry })));

    let resolution = resolver.resolve("local", PLAIN_TENANT).expect("resolves");

    assert_eq!(resolution.alias, "localhost:18418");
}

#[test]
fn an_uppercase_or_simple_form_tenant_key_matches_the_request_tenant() {
    let mut entry = provider_json("api.openai.com", "openai");
    entry["tenant_overrides"] = json!({
        // `Uuid::from_u128(0xA1)` in upper-case simple form.
        "000000000000000000000000000000A1": { "host": "tenant-a.openai.example.com" },
        "00000000-0000-0000-0000-0000000000B2": { "host": "tenant-b.openai.example.com" },
    });
    let resolver = ProviderResolver::new(&registry(&json!({ "openai": entry })));

    let a = resolver.resolve("openai", OVERRIDE_TENANT).expect("a");
    let b = resolver.resolve("openai", PLAIN_TENANT).expect("b");

    assert_eq!(a.alias, "tenant-a.openai.example.com");
    assert_eq!(b.alias, "tenant-b.openai.example.com");
}

#[test]
fn the_override_only_applies_to_its_tenant() {
    let resolver = resolver();

    let overridden = resolver.resolve("openai", OVERRIDE_TENANT).expect("a");
    let other = resolver.resolve("openai", PLAIN_TENANT).expect("b");

    assert_ne!(overridden.alias, other.alias);
    assert_eq!(other.alias, "api.openai.com");
}

#[test]
fn an_override_keeps_the_kind_and_api_path_of_its_entry() {
    let resolution = resolver()
        .resolve("openai", OVERRIDE_TENANT)
        .expect("resolves");

    assert_eq!(resolution.kind, ProviderKind::OpenaiResponses);
    assert_eq!(resolution.api_path, "/v1/responses");
}

#[test]
fn the_api_path_keeps_the_model_placeholder_and_query() {
    let resolution = resolver().resolve("azure", PLAIN_TENANT).expect("resolves");

    assert_eq!(
        resolution.api_path,
        "/openai/deployments/{model}/chat/completions?api-version=2025-01-01"
    );
}

#[test]
fn the_storage_provider_is_the_rag_provider_when_configured() {
    let resolution = resolver().resolve("azure", PLAIN_TENANT).expect("resolves");

    assert_eq!(resolution.storage_provider_id, "openai");
}

#[test]
fn the_storage_provider_is_the_entry_itself_without_a_rag_provider() {
    let resolution = resolver()
        .resolve("openai", PLAIN_TENANT)
        .expect("resolves");

    assert_eq!(resolution.storage_provider_id, "openai");

    let overridden = resolver()
        .resolve("openai", OVERRIDE_TENANT)
        .expect("resolves");
    assert_eq!(overridden.storage_provider_id, "openai");
}

#[test]
fn the_storage_provider_of_an_entry_with_a_rag_provider_ignores_the_tenant() {
    let resolution = resolver()
        .resolve("azure", OVERRIDE_TENANT)
        .expect("resolves");

    assert_eq!(resolution.storage_provider_id, "openai");
}
