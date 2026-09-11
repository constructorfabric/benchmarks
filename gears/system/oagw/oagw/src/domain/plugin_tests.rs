//! Tests for the plugin catalog and the binding rules.

use crate::domain::plugin::{
    builtin_kind, plugin_id, validate_bindings, CATALOG_ONLY_AUTH_PLUGINS,
    CATALOG_ONLY_GUARD_PLUGINS, CATALOG_ONLY_TRANSFORM_PLUGINS, AUTH_PLUGINS, BINDABLE_GUARD_PLUGINS,
    BINDABLE_TRANSFORM_PLUGINS, PluginBinding, PluginKind, Stage,
};

#[test]
fn the_plugin_identifier_is_prefixed_by_the_gts_type() {
    let id = plugin_id(uuid::Uuid::nil());
    assert!(id.starts_with("gts.cf.core.oagw.plugin.v1~"), "{id}");
}

#[test]
fn every_builtin_identifier_resolves_to_a_kind() {
    for name in AUTH_PLUGINS
        .iter()
        .chain(CATALOG_ONLY_AUTH_PLUGINS.iter())
    {
        assert_eq!(builtin_kind(name), Some(PluginKind::Auth), "{name}");
    }
    for name in BINDABLE_GUARD_PLUGINS
        .iter()
        .chain(CATALOG_ONLY_GUARD_PLUGINS.iter())
    {
        assert_eq!(builtin_kind(name), Some(PluginKind::Guard), "{name}");
    }
    for name in BINDABLE_TRANSFORM_PLUGINS
        .iter()
        .chain(CATALOG_ONLY_TRANSFORM_PLUGINS.iter())
    {
        assert_eq!(builtin_kind(name), Some(PluginKind::Transform), "{name}");
    }
}

#[test]
fn an_unknown_identifier_resolves_to_nothing() {
    assert_eq!(builtin_kind("nonexistent"), None);
    assert!(!crate::domain::plugin::is_builtin("nonexistent"));
}

#[test]
fn every_catalog_only_identifier_is_a_builtin() {
    for name in CATALOG_ONLY_AUTH_PLUGINS
        .iter()
        .chain(CATALOG_ONLY_GUARD_PLUGINS.iter())
        .chain(CATALOG_ONLY_TRANSFORM_PLUGINS.iter())
    {
        assert!(
            crate::domain::plugin::is_builtin(name),
            "{name} must be catalogued"
        );
    }
}

#[test]
fn a_known_binding_is_accepted() {
    let bindings = vec![
        PluginBinding {
            name: "apikey".to_owned(),
            uuid: None,
            config: serde_json::json!({ "credential": "cred://key" })
                .as_object()
                .cloned()
                .expect("an object"),
        },
        PluginBinding {
            name: "required_headers".to_owned(),
            uuid: None,
            config: serde_json::json!({ "headers": ["x-tenant"] })
                .as_object()
                .cloned()
                .expect("an object"),
        },
        PluginBinding {
            name: "request_id".to_owned(),
            uuid: None,
            config: serde_json::Map::new(),
        },
    ];
    assert!(validate_bindings(&bindings, Stage::Upstream).is_ok());
    assert!(validate_bindings(&bindings, Stage::Route).is_ok());
}

#[test]
fn a_catalog_only_guard_cannot_be_bound() {
    for name in CATALOG_ONLY_GUARD_PLUGINS {
        let bindings = vec![PluginBinding {
            name: name.to_owned(),
            uuid: None,
            config: serde_json::Map::new(),
        }];
        let err = validate_bindings(&bindings, Stage::Upstream)
            .expect_err("a reserved guard is not bindable");
        assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError, "{name}");
    }
}

#[test]
fn an_unknown_identifier_is_rejected_unless_it_is_a_uuid_reference() {
    let bindings = vec![PluginBinding {
        name: "made_up".to_owned(),
        uuid: None,
        config: serde_json::Map::new(),
    }];
    let err = validate_bindings(&bindings, Stage::Upstream)
        .expect_err("an unknown name is rejected");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);

    // A UUID reference is accepted; resolving it is the store's job.
    let referenced = vec![PluginBinding {
        name: "not-a-builtin".to_owned(),
        uuid: Some(uuid::Uuid::new_v4().to_string()),
        config: serde_json::Map::new(),
    }];
    assert!(validate_bindings(&referenced, Stage::Upstream).is_ok());
}

#[test]
fn a_uuid_string_is_accepted_as_a_plugin_reference() {
    let referenced = vec![PluginBinding {
        name: uuid::Uuid::new_v4().to_string(),
        uuid: None,
        config: serde_json::Map::new(),
    }];
    assert!(validate_bindings(&referenced, Stage::Route).is_ok());
}

#[test]
fn the_binding_kinds_do_not_overlap() {
    let auth = AUTH_PLUGINS
        .iter()
        .chain(BINDABLE_GUARD_PLUGINS.iter())
        .chain(BINDABLE_TRANSFORM_PLUGINS.iter());
    let mut seen = std::collections::HashSet::new();
    for name in auth {
        assert!(seen.insert(*name), "{name} appears twice in the catalog");
    }
}
