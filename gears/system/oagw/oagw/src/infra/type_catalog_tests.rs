//! The GTS catalog OAGW publishes at boot.

use super::*;

#[test]
fn every_catalog_entity_carries_a_gts_id() {
    let entities = catalog_entities();
    assert!(!entities.is_empty());
    for entity in &entities {
        let id = entity["$id"].as_str().expect("every entity needs an $id");
        assert!(id.starts_with("gts://"), "{id}");
        assert!(id.ends_with('~'), "type schemas end with the chain marker: {id}");
    }
}

#[test]
fn the_published_resource_schemas_are_the_documented_ones() {
    let entities = catalog_entities();
    let upstream = entities
        .iter()
        .find(|entity| entity["$id"] == format!("gts://{}", gts::UPSTREAM_BASE))
        .expect("the upstream schema is published");
    // Carried verbatim from docs/schemas/upstream.v1.schema.json.
    assert_eq!(upstream["title"], "OAGW Upstream Service");
    assert!(upstream["properties"]["server"].is_object());

    let route = entities
        .iter()
        .find(|entity| entity["$id"] == format!("gts://{}", gts::ROUTE_BASE))
        .expect("the route schema is published");
    assert_eq!(route["title"], "OAGW Route");
}

#[test]
fn the_plugin_and_protocol_base_types_are_published() {
    let ids: Vec<String> = catalog_entities()
        .iter()
        .map(|entity| entity["$id"].as_str().unwrap_or_default().to_owned())
        .collect();
    for base in [
        gts::AUTH_PLUGIN_BASE,
        gts::GUARD_PLUGIN_BASE,
        gts::TRANSFORM_PLUGIN_BASE,
        gts::PROTOCOL_BASE,
    ] {
        assert!(ids.contains(&format!("gts://{base}")), "{base} must be published");
    }
}
