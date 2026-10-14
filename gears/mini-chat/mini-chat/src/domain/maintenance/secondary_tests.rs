//! The deterministic alias of the Anthropic secondary file delete of chat cleanup: the model of
//! the chat in the policy snapshot catalog (a disabled model still names its provider), the
//! provider resolved for the chat's tenant with its override, accepted only for the kind
//! `anthropic_messages`, and the fallback to the tenant's single `anthropic_messages` provider
//! when the snapshot is unreadable, the model left the catalog, the provider cannot be resolved
//! or is not Anthropic, with no guess between none or several.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-secondary-file` (`inst-sf-resolve`,
//! `inst-sf-snapshot`, `inst-sf-model`, `inst-sf-provider`, `inst-sf-fallback`); definition of done
//! `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::PolicySnapshot;
use serde_json::{Value, json};

use super::resolve_secondary_alias;
use crate::test_support::maintenance::{
    ANTHROPIC_ALIAS, AZURE_ALIAS, catalog, catalog_with, catalog_without, providers_json,
    registry_of, summary_entry,
};
use crate::test_support::send::{ANTHROPIC_MODEL, CC_MODEL};
use crate::test_support::{TENANT_A, TENANT_B};

fn resolve(
    snapshot: Option<&PolicySnapshot>,
    config: &Value,
    tenant: uuid::Uuid,
    model: &str,
) -> Option<String> {
    resolve_secondary_alias(snapshot, &registry_of(config.clone()), tenant, model)
}

fn with_second_anthropic() -> Value {
    let mut config = providers_json();
    config["providers"]["anthropic_eu"] = json!({
        "kind": "anthropic_messages", "host": "anthropic-eu.example.com",
        "storage_kind": "openai", "api_path": "/v1/messages"
    });
    config
}

fn without_anthropic() -> Value {
    let mut config = providers_json();
    config["providers"]
        .as_object_mut()
        .unwrap()
        .remove("anthropic");
    config
}

#[test]
fn the_provider_of_the_chat_model_gives_the_alias() {
    // inst-sf-model, inst-sf-provider
    let snapshot = catalog();
    assert_eq!(
        resolve(
            Some(&snapshot),
            &providers_json(),
            TENANT_A,
            ANTHROPIC_MODEL
        )
        .as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn a_tenant_override_changes_the_alias() {
    // inst-sf-provider: the provider is resolved for the chat's tenant, overrides applied
    let mut config = providers_json();
    config["providers"]["anthropic"]["tenant_overrides"] =
        json!({ TENANT_A.to_string(): { "host": "tenant-a-anthropic.example.com" } });
    let snapshot = catalog();
    assert_eq!(
        resolve(Some(&snapshot), &config, TENANT_A, ANTHROPIC_MODEL).as_deref(),
        Some("tenant-a-anthropic.example.com")
    );
    assert_eq!(
        resolve(Some(&snapshot), &config, TENANT_B, ANTHROPIC_MODEL).as_deref(),
        Some(ANTHROPIC_ALIAS),
        "another tenant keeps the entry alias"
    );
}

#[test]
fn a_disabled_model_still_names_its_provider() {
    // inst-sf-model: looked up without the enabled filter; two Anthropic providers exist, so a
    // fallback would find no target
    let mut disabled =
        serde_json::to_value(summary_entry("legacy", "anthropic", &json!({}))).unwrap();
    disabled["enabled"] = json!(false);
    let snapshot = catalog_with(serde_json::from_value(disabled).unwrap());
    assert_eq!(
        resolve(
            Some(&snapshot),
            &with_second_anthropic(),
            TENANT_A,
            "legacy"
        )
        .as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn the_model_decides_between_several_anthropic_providers() {
    // inst-sf-provider: deterministic, never a guess
    let mut second = providers_json();
    second["providers"]["anthropic_eu"] = json!({
        "kind": "anthropic_messages", "host": "anthropic-eu.example.com",
        "storage_kind": "openai", "api_path": "/v1/messages"
    });
    let on_eu = summary_entry("eu-model", "anthropic_eu", &json!({}));
    let snapshot = catalog_with(on_eu);
    assert_eq!(
        resolve(Some(&snapshot), &second, TENANT_A, "eu-model").as_deref(),
        Some("anthropic-eu.example.com")
    );
    assert_eq!(
        resolve(Some(&snapshot), &second, TENANT_A, ANTHROPIC_MODEL).as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn a_model_that_left_the_catalog_falls_back_to_the_single_anthropic_provider() {
    // inst-sf-fallback
    let snapshot = catalog_without(ANTHROPIC_MODEL);
    assert_eq!(
        resolve(
            Some(&snapshot),
            &providers_json(),
            TENANT_A,
            ANTHROPIC_MODEL
        )
        .as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
    assert_eq!(
        resolve(
            Some(&snapshot),
            &providers_json(),
            TENANT_A,
            "never-existed"
        )
        .as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn a_provider_that_is_not_anthropic_falls_back_to_the_single_anthropic_provider() {
    // inst-sf-provider, inst-sf-fallback: the alias is accepted only for `anthropic_messages`
    let snapshot = catalog();
    for model in [CC_MODEL, crate::test_support::quota::STANDARD] {
        assert_eq!(
            resolve(Some(&snapshot), &providers_json(), TENANT_A, model).as_deref(),
            Some(ANTHROPIC_ALIAS),
            "{model}"
        );
    }
    // never the alias of the non-Anthropic provider
    assert_ne!(
        resolve(Some(&snapshot), &providers_json(), TENANT_A, CC_MODEL).as_deref(),
        Some(AZURE_ALIAS)
    );
}

#[test]
fn an_unreadable_snapshot_falls_back_to_the_single_anthropic_provider() {
    // inst-sf-snapshot, inst-sf-fallback
    assert_eq!(
        resolve(None, &providers_json(), TENANT_A, ANTHROPIC_MODEL).as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn an_unresolvable_model_provider_falls_back() {
    // inst-sf-provider, inst-sf-fallback: the catalog entry names a provider the registry lacks
    let ghost = summary_entry("ghost-model", "ghost", &json!({}));
    let snapshot = catalog_with(ghost);
    assert_eq!(
        resolve(Some(&snapshot), &providers_json(), TENANT_A, "ghost-model").as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn the_fallback_applies_the_tenant_override_of_the_single_anthropic_provider() {
    // inst-sf-fallback: every entry is resolved for the chat's tenant
    let mut config = providers_json();
    config["providers"]["anthropic"]["tenant_overrides"] =
        json!({ TENANT_A.to_string(): { "host": "tenant-a-anthropic.example.com" } });
    assert_eq!(
        resolve(None, &config, TENANT_A, ANTHROPIC_MODEL).as_deref(),
        Some("tenant-a-anthropic.example.com")
    );
    assert_eq!(
        resolve(None, &config, TENANT_B, ANTHROPIC_MODEL).as_deref(),
        Some(ANTHROPIC_ALIAS)
    );
}

#[test]
fn several_anthropic_providers_give_no_target_on_the_fallback() {
    // inst-sf-fallback: a guess between several Anthropic accounts is never made
    let config = with_second_anthropic();
    let snapshot = catalog_without(ANTHROPIC_MODEL);
    assert_eq!(
        resolve(Some(&snapshot), &config, TENANT_A, ANTHROPIC_MODEL),
        None
    );
    assert_eq!(resolve(None, &config, TENANT_A, ANTHROPIC_MODEL), None);
    assert_eq!(
        resolve(Some(&catalog()), &config, TENANT_A, CC_MODEL),
        None,
        "the model is not Anthropic and two accounts exist"
    );
}

#[test]
fn no_anthropic_provider_gives_no_target() {
    // inst-sf-fallback
    let config = without_anthropic();
    assert_eq!(
        resolve(Some(&catalog()), &config, TENANT_A, ANTHROPIC_MODEL),
        None
    );
    assert_eq!(resolve(None, &config, TENANT_A, ANTHROPIC_MODEL), None);
    assert_eq!(resolve(Some(&catalog()), &config, TENANT_A, CC_MODEL), None);
}

#[test]
fn the_resolution_does_not_depend_on_the_chat_model_when_it_falls_back() {
    // inst-sf-fallback: same answer for any model name once the snapshot is unreadable
    for model in ["a", "b", "", ANTHROPIC_MODEL] {
        assert_eq!(
            resolve(None, &providers_json(), TENANT_A, model).as_deref(),
            Some(ANTHROPIC_ALIAS),
            "{model:?}"
        );
    }
}
