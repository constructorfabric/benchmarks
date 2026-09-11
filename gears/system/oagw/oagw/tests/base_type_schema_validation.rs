//! Integration tests of the base-type schema documents against the GTS model
//! the `types-registry` gear validates with
//! (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`).
//!
//! The in-crate `FakeTypesRegistry` records what the gear submits without
//! checking it, so a schema document that the real registry would refuse is
//! invisible to the provisioning tests. This file runs the same documents
//! through the `gts` crate's own ingest — the code the registry runs — so a
//! document that cannot be keyed as a Type Schema fails here, at build time,
//! rather than at server start.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-type-provisioning:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gts::GtsOps;
use oagw::domain::gts_helpers::BASE_TYPES;
use oagw::infra::type_provisioning::base_type_schema;

/// Every base type is ingestable by the GTS model as a Type Schema: it is keyed
/// by its identifier, and its document passes the full validation pipeline the
/// registry runs on a `validate` ingest.
#[test]
fn every_base_type_schema_is_a_keyable_type_schema() {
    let mut ops = GtsOps::new(None, None, 0);
    for id in BASE_TYPES {
        let schema = base_type_schema(id);
        let result = ops.add_entity(&schema, true);
        assert!(result.ok, "`{id}` was refused: {}", result.error);
        assert_eq!(result.id, *id, "`{id}` is keyed by its own identifier");
    }
}

/// A schema whose `$id` carries the bare canonical form instead of the
/// `gts://` URI form is refused: that is the failure this suite exists to
/// catch, and the regression it guards.
#[test]
fn a_bare_canonical_id_in_dollar_id_is_not_a_keyable_schema() {
    let mut ops = GtsOps::new(None, None, 0);
    let mut schema = base_type_schema(BASE_TYPES[0]);
    schema["$id"] = serde_json::json!(BASE_TYPES[0]);
    let result = ops.add_entity(&schema, true);
    assert!(!result.ok, "the bare form is not a GTS id");
}
