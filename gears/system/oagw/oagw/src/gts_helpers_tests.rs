//! Unit tests for the GTS identifier helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn protocol_ids_match_the_wire_contract() {
    assert_eq!(
        PROTOCOL_HTTP,
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    );
    assert_eq!(
        PROTOCOL_GRPC,
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
    );
}

#[test]
fn built_in_plugin_ids_carry_their_type_prefix() {
    assert!(AUTH_APIKEY.starts_with(AUTH_PLUGIN_TYPE));
    assert!(GUARD_REQUIRED_HEADERS.starts_with(GUARD_PLUGIN_TYPE));
    assert!(TRANSFORM_REQUEST_ID.starts_with(TRANSFORM_PLUGIN_TYPE));
    assert!(AUTH_BASIC.starts_with(AUTH_PLUGIN_TYPE));
    assert!(GUARD_TIMEOUT.starts_with(GUARD_PLUGIN_TYPE));
    assert!(TRANSFORM_LOGGING.starts_with(TRANSFORM_PLUGIN_TYPE));
}

#[test]
fn error_type_ids_are_namespaced_under_the_oagw_prefix() {
    assert_eq!(
        error_type_id("validation.error"),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}
