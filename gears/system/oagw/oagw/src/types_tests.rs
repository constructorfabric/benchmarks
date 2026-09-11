//! Tests for the gear's GTS type catalog.

use crate::types::{
    catalog, plugin_instance_id, BINDABLE_PLUGINS, CATALOG_ONLY_PLUGINS, ERROR_TYPE, HTTP_PROTOCOL_TYPE,
    PLUGIN_TYPE, ROUTE_TYPE, UPSTREAM_TYPE,
};

#[test]
fn the_entity_type_identifiers_are_gts_qualified() {
    assert!(UPSTREAM_TYPE.starts_with("gts.cf.core.oagw.upstream.v1"));
    assert!(ROUTE_TYPE.starts_with("gts.cf.core.oagw.route.v1"));
    assert!(PLUGIN_TYPE.starts_with("gts.cf.core.oagw.plugin.v1"));
    assert!(ERROR_TYPE.starts_with("gts.cf.core.errors.err.v1"));
}

#[test]
fn the_protocol_bindings_are_derived_from_the_protocol_type() {
    for id in [HTTP_PROTOCOL_TYPE] {
        assert!(id.starts_with("gts.cf.core.oagw.protocol.v1~"), "{id}");
    }
    assert_ne!(HTTP_PROTOCOL_TYPE, "");
}

#[test]
fn every_plugin_has_an_instance_identifier_derived_from_the_type() {
    for name in BINDABLE_PLUGINS.iter().chain(CATALOG_ONLY_PLUGINS.iter()) {
        let id = plugin_instance_id(name);
        assert!(id.starts_with(PLUGIN_TYPE), "{id}");
        assert!(id.ends_with(&format!("cf.core.oagw.plugin_{name}.v1")), "{id}");
    }
}

#[test]
fn instance_identifiers_are_unique() {
    let ids: std::collections::HashSet<String> = BINDABLE_PLUGINS
        .iter()
        .chain(CATALOG_ONLY_PLUGINS.iter())
        .map(|name| plugin_instance_id(name))
        .collect();
    assert_eq!(
        ids.len(),
        BINDABLE_PLUGINS.len() + CATALOG_ONLY_PLUGINS.len(),
        "two plugins share an instance identifier"
    );
}

#[test]
fn the_catalog_lists_every_plugin_with_a_binding_verdict() {
    let entries = catalog();
    assert_eq!(
        entries.len(),
        BINDABLE_PLUGINS.len() + CATALOG_ONLY_PLUGINS.len()
    );
    for name in BINDABLE_PLUGINS {
        let entry = entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} is not catalogued"));
        assert!(entry.bindable, "{name} must be bindable");
    }
    for name in CATALOG_ONLY_PLUGINS {
        let entry = entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} is not catalogued"));
        assert!(!entry.bindable, "{name} must be catalogued only");
    }
}

#[test]
fn the_catalog_renders_in_a_stable_order() {
    let first: Vec<String> = catalog().into_iter().map(|entry| entry.name).collect();
    let second: Vec<String> = catalog().into_iter().map(|entry| entry.name).collect();
    assert_eq!(first, second);
    assert_eq!(first[0], "noop", "the bindable plugins come first");
}

#[test]
fn the_reserved_identifiers_are_catalogued() {
    let entries = catalog();
    for name in ["basic", "bearer", "timeout", "cors", "logging", "metrics"] {
        assert!(
            entries.iter().any(|entry| entry.name == name),
            "{name} is missing from the catalog"
        );
    }
}
