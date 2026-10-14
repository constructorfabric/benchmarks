//! Provider registry tests: the default `openai` entry, the alias fill, the per-field
//! override fallback, the storage and API-version rules and the resolution error.
//!
//! Slice S2: algorithm `provider-registry`, algorithm `gear-init` step `inst-init-build-registry`,
//! definition of done `provider-provisioning`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use uuid::Uuid;

use super::{ProviderRegistry, ProviderResolutionError};
use crate::config::{MiniChatConfig, ProviderKind, StorageKind};
use crate::domain::error::DomainError;
use crate::test_support::{TENANT_A, TENANT_B};

fn registry_of(providers: Value) -> ProviderRegistry {
    let mut config = json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" }
    });
    if !providers.is_null() {
        config["providers"] = providers;
    }
    let parsed = MiniChatConfig::from_value(config).expect("valid config");
    ProviderRegistry::build(&parsed.providers)
}

// ── default entry ────────────────────────────────────────────────────────────

#[test]
fn an_absent_providers_section_yields_the_single_default_openai_entry() {
    let registry = registry_of(Value::Null);
    let entries: Vec<_> = registry.entries().collect();
    assert_eq!(entries.len(), 1);
    let base = &entries[0].base;
    assert_eq!(base.provider_id, "openai");
    assert_eq!(base.kind, ProviderKind::OpenaiResponses);
    assert_eq!(base.host, "api.openai.com");
    assert_eq!(base.port, 443);
    assert!(!base.use_http);
    assert_eq!(base.storage_kind, StorageKind::Openai);
    assert_eq!(base.storage_backend, "openai");
    assert_eq!(base.api_path, "/v1/responses");
    // API-key auth, credentials by reference only
    assert_eq!(
        base.auth_plugin_type.as_deref(),
        Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
    );
    assert_eq!(
        base.auth_config.get("secret_ref").map(String::as_str),
        Some("cred://openai-key")
    );
    assert!(entries[0].overrides.is_empty());
}

#[test]
fn the_default_entry_gets_its_host_as_the_oagw_alias() {
    let registry = registry_of(Value::Null);
    assert_eq!(
        registry.entry("openai").unwrap().base.upstream_alias,
        "api.openai.com"
    );
}

// ── entry defaults and alias ─────────────────────────────────────────────────

#[test]
fn entry_defaults_are_applied() {
    let registry = registry_of(json!({
        "main": { "kind": "vllm_responses", "host": "llm.internal", "storage_kind": "openai" }
    }));
    let base = &registry.entry("main").unwrap().base;
    assert_eq!(base.provider_id, "main");
    assert_eq!(base.kind, ProviderKind::VllmResponses);
    assert_eq!(base.port, 443);
    assert!(!base.use_http);
    assert_eq!(base.api_path, "/v1/responses");
    assert_eq!(
        base.storage_backend, "main",
        "storage_backend defaults to the id"
    );
    assert_eq!(base.api_version, None);
    assert_eq!(base.rag_provider, None);
}

#[test]
fn use_http_defaults_the_port_to_80_and_an_explicit_port_wins() {
    let registry = registry_of(json!({
        "plain": { "kind": "openai_responses", "host": "127.0.0.1", "use_http": true, "storage_kind": "openai" },
        "custom": { "kind": "openai_responses", "host": "h.example.com", "port": 8443, "storage_kind": "openai" },
        "plain_custom": { "kind": "openai_responses", "host": "127.0.0.1", "use_http": true, "port": 8080, "storage_kind": "openai" }
    }));
    assert_eq!(registry.entry("plain").unwrap().base.port, 80);
    assert!(registry.entry("plain").unwrap().base.use_http);
    assert_eq!(registry.entry("custom").unwrap().base.port, 8443);
    assert_eq!(registry.entry("plain_custom").unwrap().base.port, 8080);
}

#[test]
fn a_configured_storage_backend_label_wins_over_the_provider_id() {
    let registry = registry_of(json!({
        "main": { "kind": "openai_responses", "host": "h.example.com", "storage_kind": "openai", "storage_backend": "files-eu" }
    }));
    assert_eq!(
        registry.entry("main").unwrap().base.storage_backend,
        "files-eu"
    );
}

#[test]
fn upstream_alias_defaults_to_the_host_and_a_configured_alias_wins() {
    let registry = registry_of(json!({
        "unaliased": { "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" },
        "aliased": { "kind": "openai_responses", "host": "10.0.0.5", "upstream_alias": "llm-main", "storage_kind": "openai" },
        "bracketed": { "kind": "openai_responses", "host": "host_1.example.com:8443", "storage_kind": "openai" }
    }));
    assert_eq!(
        registry.entry("unaliased").unwrap().base.upstream_alias,
        "api.example.com"
    );
    assert_eq!(
        registry.entry("aliased").unwrap().base.upstream_alias,
        "llm-main"
    );
    // the alias is the host verbatim: no port is appended or stripped
    assert_eq!(
        registry.entry("bracketed").unwrap().base.upstream_alias,
        "host_1.example.com:8443"
    );
}

#[test]
fn upstream_alias_is_the_alias_oagw_derives_from_the_endpoint() {
    // (provider entry, expected alias): OAGW accepts only the derived alias on a hostname
    // endpoint (host, plus `:port` when the port is not 80 for http or 443 for https) and any
    // alias on an IP endpoint
    let cases = [
        (
            json!({ "host": "localhost", "port": 9999, "use_http": true }),
            "localhost:9999",
        ),
        (
            json!({ "host": "api.openai.com", "port": 443 }),
            "api.openai.com",
        ),
        (json!({ "host": "api.openai.com" }), "api.openai.com"),
        (
            json!({ "host": "llm.internal", "use_http": true }),
            "llm.internal",
        ),
        (
            json!({ "host": "llm.internal", "port": 443, "use_http": true }),
            "llm.internal:443",
        ),
        (
            json!({ "host": "api.openai.com", "port": 80 }),
            "api.openai.com:80",
        ),
        (
            json!({ "host": "LLM.Internal.", "port": 8080 }),
            "llm.internal:8080",
        ),
        (
            json!({ "host": "127.0.0.1", "port": 9999, "use_http": true }),
            "127.0.0.1",
        ),
    ];
    for (entry, expected) in cases {
        let mut entry = entry;
        entry["kind"] = json!("openai_responses");
        entry["storage_kind"] = json!("openai");
        let registry = registry_of(json!({ "p": entry.clone() }));
        assert_eq!(
            registry.entry("p").unwrap().base.upstream_alias,
            expected,
            "{entry}"
        );
    }
}

#[test]
fn a_tenant_override_host_gets_the_alias_derived_for_its_own_host() {
    let registry = registry_of(json!({
        "p": {
            "kind": "openai_responses", "host": "localhost", "port": 9999, "use_http": true,
            "storage_kind": "openai",
            "tenant_overrides": { (tenant_key(TENANT_A)): { "host": "other.local" } }
        }
    }));
    let entry = registry.entry("p").unwrap();
    assert_eq!(entry.base.upstream_alias, "localhost:9999");
    assert_eq!(
        entry.overrides[&tenant_key(TENANT_A)].upstream_alias,
        "other.local:9999"
    );
}

// ── storage kind and API version ─────────────────────────────────────────────

#[test]
fn an_azure_entry_keeps_its_api_version_and_storage_kind() {
    let registry = registry_of(json!({
        "azure": {
            "kind": "openai_responses", "host": "res.openai.azure.com", "storage_kind": "azure",
            "api_version": "2025-03-01-preview", "api_path": "/openai/v1/responses?api-version=preview"
        }
    }));
    let base = &registry.entry("azure").unwrap().base;
    assert_eq!(base.storage_kind, StorageKind::Azure);
    assert_eq!(base.api_version.as_deref(), Some("2025-03-01-preview"));
    assert_eq!(base.api_path, "/openai/v1/responses?api-version=preview");
}

#[test]
fn the_rag_provider_reference_is_kept() {
    let registry = registry_of(json!({
        "claude": { "kind": "anthropic_messages", "host": "api.anthropic.com", "storage_kind": "openai", "rag_provider": "openai" },
        "openai": { "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai" }
    }));
    assert_eq!(
        registry
            .entry("claude")
            .unwrap()
            .base
            .rag_provider
            .as_deref(),
        Some("openai")
    );
}

#[test]
fn every_adapter_kind_is_selectable_through_the_registry() {
    let registry = registry_of(json!({
        "a": { "kind": "openai_responses", "host": "a.example.com", "storage_kind": "openai" },
        "b": { "kind": "openai_chat_completions", "host": "b.example.com", "storage_kind": "openai" },
        "c": { "kind": "vllm_responses", "host": "c.example.com", "storage_kind": "openai" },
        "d": { "kind": "anthropic_messages", "host": "d.example.com", "storage_kind": "openai" }
    }));
    let kind = |id: &str| registry.resolve(id, TENANT_A).unwrap().kind;
    assert_eq!(kind("a"), ProviderKind::OpenaiResponses);
    assert_eq!(kind("b"), ProviderKind::OpenaiChatCompletions);
    assert_eq!(kind("c"), ProviderKind::VllmResponses);
    assert_eq!(kind("d"), ProviderKind::AnthropicMessages);
}

// ── tenant overrides ─────────────────────────────────────────────────────────

fn tenant_key(tenant: Uuid) -> String {
    tenant.to_string()
}

fn registry_with_overrides() -> ProviderRegistry {
    registry_of(json!({
        "main": {
            "kind": "openai_responses", "host": "10.0.0.5", "port": 8443,
            "storage_kind": "openai", "upstream_alias": "main-alias",
            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "auth_config": { "header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://main-key" },
            "tenant_overrides": {
                (tenant_key(TENANT_A)): { "host": "tenant-a.example.com" },
                (tenant_key(TENANT_B)): {
                    "upstream_alias": "tenant-b-alias",
                    "auth_config": { "header": "api-key", "prefix": "", "secret_ref": "cred://tenant-b-key" }
                }
            }
        }
    }))
}

#[test]
fn an_override_that_sets_only_a_host_gets_that_host_as_its_alias_and_inherits_the_rest() {
    let registry = registry_with_overrides();
    let entry = registry.entry("main").unwrap();
    let over = &entry.overrides[&tenant_key(TENANT_A)];
    assert_eq!(over.host, "tenant-a.example.com");
    // an unset override alias is filled with the alias derived for the override host (port
    // 8443 is non-standard, so OAGW requires it in the alias), not the entry alias
    assert_eq!(over.upstream_alias, "tenant-a.example.com:8443");
    // every other field falls back to the entry on its own
    assert_eq!(over.port, 8443);
    assert_eq!(over.kind, ProviderKind::OpenaiResponses);
    assert_eq!(over.api_path, entry.base.api_path);
    assert_eq!(over.auth_plugin_type, entry.base.auth_plugin_type);
    assert_eq!(
        over.auth_config, entry.base.auth_config,
        "the entry credentials reference is sent to the tenant host"
    );
    assert_eq!(over.provider_id, "main");
}

#[test]
fn an_override_that_sets_only_an_alias_keeps_the_entry_host_and_replaces_the_auth_config() {
    let registry = registry_with_overrides();
    let entry = registry.entry("main").unwrap();
    let over = &entry.overrides[&tenant_key(TENANT_B)];
    assert_eq!(over.upstream_alias, "tenant-b-alias");
    assert_eq!(over.host, "10.0.0.5", "host falls back to the entry");
    assert_eq!(
        over.auth_config.get("secret_ref").map(String::as_str),
        Some("cred://tenant-b-key")
    );
    assert_eq!(over.auth_plugin_type, entry.base.auth_plugin_type);
}

#[test]
fn the_entry_itself_is_not_changed_by_its_overrides() {
    let registry = registry_with_overrides();
    let base = &registry.entry("main").unwrap().base;
    assert_eq!(base.host, "10.0.0.5");
    assert_eq!(base.upstream_alias, "main-alias");
    assert_eq!(
        base.auth_config.get("secret_ref").map(String::as_str),
        Some("cred://main-key")
    );
}

#[test]
fn resolve_returns_the_tenant_override_or_falls_back_to_the_entry() {
    let registry = registry_with_overrides();
    assert_eq!(
        registry.resolve("main", TENANT_A).unwrap().upstream_alias,
        "tenant-a.example.com:8443"
    );
    assert_eq!(
        registry.resolve("main", TENANT_B).unwrap().upstream_alias,
        "tenant-b-alias"
    );
    let other_tenant = Uuid::from_u128(0x00C0_FFEE);
    assert_eq!(
        registry
            .resolve("main", other_tenant)
            .unwrap()
            .upstream_alias,
        "main-alias"
    );
}

// ── resolution error ─────────────────────────────────────────────────────────

#[test]
fn an_unknown_provider_is_a_resolution_error_that_maps_to_internal() {
    let registry = registry_of(Value::Null);
    let error = registry.resolve("removed-provider", TENANT_A).unwrap_err();
    assert_eq!(
        error,
        ProviderResolutionError::UnknownProvider("removed-provider".to_owned())
    );
    let domain = DomainError::from(error);
    assert!(matches!(domain, DomainError::Internal { .. }), "{domain:?}");
}

// ── credentials by reference ─────────────────────────────────────────────────

#[test]
fn credentials_are_held_by_reference_only() {
    let registry = registry_of(Value::Null);
    let auth = &registry.entry("openai").unwrap().base.auth_config;
    assert_eq!(
        auth.get("secret_ref").map(String::as_str),
        Some("cred://openai-key")
    );
    assert!(
        auth.values().all(|value| !value.starts_with("sk-")),
        "{auth:?}"
    );
}
