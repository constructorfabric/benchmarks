//! Integration tests of the plugin-catalog instance documents against the GTS
//! model the `types-registry` gear validates with
//! (`cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
//!
//! The in-crate `FakeTypesRegistry` records what the gear submits without
//! checking it, so an instance document the real registry would refuse is
//! invisible to the provisioning tests. This file runs the documents through
//! the `gts` crate's own ingest — the code the registry runs — after the base
//! types, exactly as initialization does.
//!
//! The regression this guards: a catalog document shaped as a JSON Schema (a
//! non-empty `$schema`) is classified as a Type Schema, and a Type Schema
//! keyed by an instance identifier — which carries a segment past the `~` and
//! no `gts://` URI form — is refused on ingest. The registry surfaces that as
//! `invalid_argument: Request validation failed`, and gear initialization
//! aborts.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-identifier-resolution:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gts::GtsOps;
use oagw::domain::gts_helpers::{
    AUTH_PLUGIN_BASE_TYPE, CATALOG_ONLY_PLUGIN_IDS, GUARD_PLUGIN_BASE_TYPE,
    TRANSFORM_PLUGIN_BASE_TYPE,
};
use oagw::infra::type_provisioning::base_type_schema;
use oagw::domain::type_catalog::catalog_instance_documents;

/// The three base types the catalog-only identifiers derive from.
const CATALOG_BASE_TYPES: [&str; 3] =
    [AUTH_PLUGIN_BASE_TYPE, GUARD_PLUGIN_BASE_TYPE, TRANSFORM_PLUGIN_BASE_TYPE];

/// Every catalog-only identifier registers as a GTS **instance** of its base
/// type: the parent type schema is provisioned first, exactly as
/// initialization does, and each document is then keyed by its own identifier.
#[test]
fn every_catalog_identifier_registers_as_an_instance_of_its_base_type() {
    let mut ops = GtsOps::new(None, None, 0);
    for id in CATALOG_BASE_TYPES {
        let result = ops.add_entity(&base_type_schema(id), true);
        assert!(result.ok, "the base type `{id}` was refused: {}", result.error);
    }

    for id in CATALOG_ONLY_PLUGIN_IDS {
        let document = catalog_instance_documents()
            .into_iter()
            .find(|document| document["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("`{id}` is cataloged"));
        let result = ops.add_entity(&document, true);
        assert!(result.ok, "`{id}` was refused: {}", result.error);
        assert_eq!(result.id, *id, "`{id}` is keyed by its own identifier");
        assert!(!result.is_type_schema, "`{id}` registers as an instance");
    }
}

/// An instance document that carries a non-empty `$schema` is classified as a
/// Type Schema and refused: that is the failure this suite exists to catch,
/// and the regression it guards.
#[test]
fn a_schema_shaped_catalog_document_is_refused() {
    let mut ops = GtsOps::new(None, None, 0);
    let base = base_type_schema(AUTH_PLUGIN_BASE_TYPE);
    assert!(ops.add_entity(&base, true).ok, "the base type registers");

    let mut document = catalog_instance_documents()
        .into_iter()
        .next()
        .expect("the catalog is non-empty");
    document["$schema"] = serde_json::json!("https://json-schema.org/draft/2020-12/schema");
    let result = ops.add_entity(&document, true);
    assert!(!result.ok, "the schema-shaped document is not an instance");
}
