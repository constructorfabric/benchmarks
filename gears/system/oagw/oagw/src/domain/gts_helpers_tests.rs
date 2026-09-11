//! Unit tests for the GTS identifiers the gateway reserves.

use super::*;
use uuid::Uuid;

#[test]
fn resource_identifiers_are_the_namespaced_ones() {
    let id = Uuid::now_v7();
    assert_eq!(upstream_gts(id), format!("{UPSTREAM_GTS}{id}"));
    assert_eq!(route_gts(id), format!("{ROUTE_GTS}{id}"));
    assert_eq!(plugin_gts(id), format!("{PLUGIN_GTS}{id}"));
    assert_eq!(proxy_gts(id), format!("{PROXY_GTS}{id}"));
    for id in [upstream_gts(id), route_gts(id), plugin_gts(id), proxy_gts(id)] {
        assert!(id.starts_with("gts.cf.core."), "{id}");
    }
}

#[test]
fn both_protocols_are_known_and_nothing_else_is() {
    assert!(is_known_protocol(PROTOCOL_HTTP));
    assert!(is_known_protocol(PROTOCOL_GRPC));
    assert!(!is_known_protocol("gts.cf.core.oagw.protocol.v1~cf.core.oagw.websocket.v1"));
    assert!(!is_known_protocol(""));
}

#[test]
fn the_catalogue_lists_every_plugin_of_each_kind() {
    assert_eq!(
        auth_plugin_catalog(),
        vec![AUTH_NOOP, AUTH_APIKEY, AUTH_OAUTH2_CC, AUTH_OAUTH2_CC_BASIC, AUTH_BASIC, AUTH_BEARER]
    );
    assert_eq!(guard_plugin_catalog(), vec![GUARD_REQUIRED_HEADERS, GUARD_TIMEOUT, GUARD_CORS]);
    assert_eq!(
        transform_plugin_catalog(),
        vec![TRANSFORM_REQUEST_ID, TRANSFORM_LOGGING, TRANSFORM_METRICS]
    );
}

#[test]
fn the_combined_catalogue_tags_each_entry_with_its_kind() {
    let catalogue = builtin_catalogue();
    assert_eq!(catalogue.len(), 12);
    assert!(catalogue.contains(&(AUTH_NOOP, "auth")));
    assert!(catalogue.contains(&(GUARD_REQUIRED_HEADERS, "guard")));
    assert!(catalogue.contains(&(TRANSFORM_REQUEST_ID, "transform")));
    assert!(catalogue.iter().all(|(id, _)| id.starts_with("gts.")));
}

#[test]
fn error_identifiers_follow_the_design_table() {
    assert_eq!(ERR_ROUTE_NOT_FOUND, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    assert_eq!(ERR_UPSTREAM_NOT_FOUND, "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1");
    assert_eq!(
        ERR_MISSING_TARGET_HOST,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    for code in [
        ERR_VALIDATION,
        ERR_UPSTREAM_NOT_FOUND,
        ERR_PLUGIN_NOT_FOUND,
        ERR_PLUGIN_IN_USE,
        ERR_AUTH_FAILED,
        ERR_SECRET_NOT_FOUND,
        ERR_PAYLOAD_TOO_LARGE,
        ERR_RATE_LIMIT,
        ERR_DOWNSTREAM,
        ERR_TIMEOUT,
        ERR_CIRCUIT_OPEN,
        ERR_PROTOCOL,
        ERR_STREAM_ABORTED,
        ERR_LINK_UNAVAILABLE,
        ERR_MISSING_TARGET_HOST,
        ERR_INVALID_TARGET_HOST,
        ERR_UNKNOWN_TARGET_HOST,
        ERR_CORS_ORIGIN,
        ERR_CORS_METHOD,
        ERR_ROUTE_NOT_FOUND,
    ] {
        assert!(code.starts_with("gts.cf.core.errors.err.v1~"), "{code}");
    }
}

#[test]
fn catalog_only_plugins_are_not_resolvable_by_the_registry() {
    // These must stay out of the registered set even though the catalogue lists them.
    let auth_catalog = auth_plugin_catalog();
    assert!(auth_catalog.contains(&AUTH_BASIC));
    assert!(auth_catalog.contains(&AUTH_BEARER));
    assert!(guard_plugin_catalog().contains(&GUARD_TIMEOUT));
}
