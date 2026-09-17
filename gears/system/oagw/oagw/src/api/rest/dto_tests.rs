//! Tests for the plugin-reference DTOs.
//!
//! A chain item crosses the wire in either spelling of `docs/ADR/0009`: a bare
//! GTS id, or a bound object carrying the plugin's per-binding configuration.
//! These tests pin both spellings, the model → DTO projection, and the fact
//! that the bound form survives a serialise → deserialise round-trip unchanged.
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::rest::dto::{OagwSharingModeDto, PluginChainDto, PluginRefDto};
use crate::domain::model::{PluginBinding, PluginsConfig, SharingMode};

/// The JSON spelling of `Uuid::nil()`, so the expected value reads on its own.
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

fn chain(items: Vec<PluginBinding>) -> PluginChainDto {
    PluginChainDto::from(&PluginsConfig {
        sharing: SharingMode::Private,
        items,
    })
}

#[test]
fn a_bare_plugin_reference_is_deserialised_from_a_plain_string() {
    let chain: PluginChainDto =
        serde_json::from_value(json!({"items": ["gts.some.plugin.v1"]})).expect("a bare chain");
    assert_eq!(chain.items.len(), 1);
    assert_eq!(
        chain.items[0],
        PluginRefDto::Bare("gts.some.plugin.v1".to_owned()),
        "the string spelling is the bare form"
    );
    assert_eq!(chain.sharing, OagwSharingModeDto::Private);
}

#[test]
fn a_bound_plugin_reference_keeps_its_config_and_leaves_the_uuid_unset() {
    let chain: PluginChainDto = serde_json::from_value(json!({
        "items": [{"plugin_ref": "gts.some.plugin.v1", "config": {"a": 1}}]
    }))
    .expect("a bound chain");
    assert_eq!(chain.items.len(), 1);
    let PluginRefDto::Bound {
        plugin_ref,
        plugin_uuid,
        config,
    } = &chain.items[0]
    else {
        panic!("the object spelling must deserialise to the bound form");
    };
    assert_eq!(plugin_ref, "gts.some.plugin.v1");
    assert_eq!(plugin_uuid, &None, "no UUID was sent");
    assert_eq!(config, &Some(json!({"a": 1})));
}

#[test]
fn a_plugin_chain_serialises_the_bare_and_the_bound_binding() {
    let bare = PluginBinding::bare("gts.some.plugin.v1");
    let bound = PluginBinding {
        plugin_ref: "gts.other.plugin.v1".to_owned(),
        plugin_uuid: None,
        config: Some(json!({"cache_ttl_secs": 30})),
    };

    let value = serde_json::to_value(chain(vec![bare, bound])).expect("a chain");
    assert_eq!(
        value["items"][0],
        json!("gts.some.plugin.v1"),
        "an unconfigured, unresolved binding is a plain string"
    );
    assert_eq!(
        value["items"][1],
        json!({"plugin_ref": "gts.other.plugin.v1", "config": {"cache_ttl_secs": 30}}),
        "a configured binding is an object"
    );
    assert!(
        value["items"][1].get("plugin_uuid").is_none(),
        "an unresolved plugin_uuid is omitted"
    );
}

#[test]
fn a_resolved_plugin_uuid_travels_with_its_binding() {
    let resolved = PluginBinding {
        plugin_ref: "gts.third.plugin.v1".to_owned(),
        plugin_uuid: Some(Uuid::nil()),
        config: None,
    };

    let value: Value = serde_json::to_value(chain(vec![resolved])).expect("a chain");
    assert_eq!(
        value["items"][0]["plugin_ref"],
        json!("gts.third.plugin.v1")
    );
    assert_eq!(value["items"][0]["plugin_uuid"], json!(NIL_UUID));
    assert!(
        value["items"][0].get("config").is_none(),
        "a binding without configuration omits it"
    );
}

#[test]
fn a_serialised_plugin_chain_survives_a_management_round_trip() {
    let chain = chain(vec![
        PluginBinding::bare("gts.some.plugin.v1"),
        PluginBinding {
            plugin_ref: "gts.other.plugin.v1".to_owned(),
            plugin_uuid: None,
            config: Some(json!({"a": 1})),
        },
    ]);

    let wire = serde_json::to_value(&chain).expect("a chain");
    let round_tripped: PluginChainDto = serde_json::from_value(wire).expect("a chain");
    assert_eq!(
        chain, round_tripped,
        "the bound form is not lost on the wire"
    );
    let PluginRefDto::Bound { plugin_uuid, .. } = &round_tripped.items[1] else {
        panic!("the bound form must come back as the bound form");
    };
    assert_eq!(plugin_uuid, &None);
}

#[test]
fn a_plugin_reference_reports_its_identifier_in_both_spellings() {
    let bare = PluginRefDto::Bare("gts.some.plugin.v1".to_owned());
    let bound = PluginRefDto::Bound {
        plugin_ref: "gts.other.plugin.v1".to_owned(),
        plugin_uuid: None,
        config: Some(json!({"a": 1})),
    };
    assert_eq!(bare.plugin_ref(), "gts.some.plugin.v1");
    assert_eq!(bound.plugin_ref(), "gts.other.plugin.v1");

    // Both spellings project onto the domain binding the engine executes.
    assert_eq!(
        PluginBinding::from(&bare),
        PluginBinding::bare("gts.some.plugin.v1")
    );
    let bound_binding = PluginBinding::from(&bound);
    assert_eq!(bound_binding.plugin_ref, "gts.other.plugin.v1");
    assert_eq!(bound_binding.plugin_uuid, None);
    assert_eq!(bound_binding.config, Some(json!({"a": 1})));
}
