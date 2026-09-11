//! Parsing of the GTS identifiers OAGW accepts on the wire.

use super::*;

const SAMPLE: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";

#[test]
fn split_instance_separates_base_and_instance() {
    let (base, instance) = split_instance(APIKEY_AUTH_PLUGIN_ID).unwrap();
    assert_eq!(base, AUTH_PLUGIN_BASE);
    assert_eq!(instance, "cf.core.oagw.apikey.v1");
}

#[test]
fn a_bare_base_type_has_no_instance() {
    assert!(split_instance(UPSTREAM_BASE).is_none());
}

#[test]
fn instance_uuid_reads_both_spellings() {
    let expected = Uuid::parse_str(SAMPLE).unwrap();
    assert_eq!(
        instance_uuid(&format!("{GUARD_PLUGIN_BASE}{SAMPLE}")),
        Some(expected)
    );
    assert_eq!(instance_uuid(SAMPLE), Some(expected));
    assert_eq!(instance_uuid(APIKEY_AUTH_PLUGIN_ID), None);
}

#[test]
fn resource_ids_accept_a_uuid_or_the_anonymous_gts_form() {
    let expected = Uuid::parse_str(SAMPLE).unwrap();
    assert_eq!(parse_resource_id(SAMPLE, Some(UPSTREAM_BASE)), Some(expected));
    assert_eq!(
        parse_resource_id(&format!("{UPSTREAM_BASE}{SAMPLE}"), Some(UPSTREAM_BASE)),
        Some(expected)
    );
}

#[test]
fn a_route_identifier_cannot_address_an_upstream() {
    assert_eq!(
        parse_resource_id(&format!("{ROUTE_BASE}{SAMPLE}"), Some(UPSTREAM_BASE)),
        None
    );
}

#[test]
fn malformed_resource_ids_are_rejected() {
    assert_eq!(parse_resource_id("not-a-uuid", Some(UPSTREAM_BASE)), None);
    assert_eq!(
        parse_resource_id(&format!("{UPSTREAM_BASE}not-a-uuid"), Some(UPSTREAM_BASE)),
        None
    );
}

#[test]
fn plugin_ids_carry_their_kind_when_written_in_full() {
    let expected = Uuid::parse_str(SAMPLE).unwrap();
    let (base, id) = parse_plugin_id(&format!("{TRANSFORM_PLUGIN_BASE}{SAMPLE}")).unwrap();
    assert_eq!(base, Some(TRANSFORM_PLUGIN_BASE));
    assert_eq!(id, expected);

    // A bare UUID is kind-agnostic.
    let (base, id) = parse_plugin_id(SAMPLE).unwrap();
    assert_eq!(base, None);
    assert_eq!(id, expected);

    // An unrelated base type is not a plugin identifier.
    assert!(parse_plugin_id(&format!("{UPSTREAM_BASE}{SAMPLE}")).is_none());
}

#[test]
fn plugin_base_matching_admits_bare_uuids() {
    assert!(matches_plugin_base(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        GUARD_PLUGIN_BASE
    ));
    assert!(!matches_plugin_base(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        AUTH_PLUGIN_BASE
    ));
    assert!(matches_plugin_base(SAMPLE, AUTH_PLUGIN_BASE));
}

#[test]
fn the_catalog_identifiers_are_the_documented_ones() {
    assert_eq!(UPSTREAM_BASE, "gts.cf.core.oagw.upstream.v1~");
    assert_eq!(ROUTE_BASE, "gts.cf.core.oagw.route.v1~");
    assert_eq!(
        PROTOCOL_HTTP,
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    );
    assert_eq!(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(
        REQUEST_ID_TRANSFORM_PLUGIN_ID,
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
}
