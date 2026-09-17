use super::*;
use uuid::Uuid;

#[test]
fn formats_and_parses_gts_ids() {
    let uuid = Uuid::parse_str("3f2504e0-4f89-11d3-9a0c-0305e82c3301").expect("valid uuid");
    let id = format_id(UPSTREAM_TYPE, uuid);
    assert_eq!(
        id,
        "gts.cf.core.oagw.upstream.v1~3f2504e0-4f89-11d3-9a0c-0305e82c3301"
    );
    assert_eq!(instance_part(&id), "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    assert_eq!(uuid_instance(&id), Some(uuid));
}

#[test]
fn instance_part_accepts_bare_uuid() {
    let bare = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
    assert_eq!(instance_part(bare), bare);
    assert!(uuid_instance(bare).is_some());
}

#[test]
fn strip_base_returns_instance() {
    let full = format!("{ROUTE_TYPE}abc");
    assert_eq!(strip_base(ROUTE_TYPE, &full), Some("abc"));
    assert_eq!(strip_base(ROUTE_TYPE, "abc"), Some("abc"));
    assert_eq!(
        strip_base(ROUTE_TYPE, "gts.cf.core.oagw.plugin.v1~abc"),
        None
    );
}

#[test]
fn builtin_plugin_names_match_contract() {
    assert_eq!(
        format!("{AUTH_PLUGIN_TYPE}{AUTH_NOOP}"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert_eq!(
        format!("{AUTH_PLUGIN_TYPE}{AUTH_APIKEY}"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        format!("{AUTH_PLUGIN_TYPE}{AUTH_OAUTH2_CLIENT_CRED}"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"
    );
    assert_eq!(
        format!("{AUTH_PLUGIN_TYPE}{AUTH_OAUTH2_CLIENT_CRED_BASIC}"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
    );
    assert_eq!(
        format!("{GUARD_PLUGIN_TYPE}{GUARD_REQUIRED_HEADERS}"),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(
        format!("{TRANSFORM_PLUGIN_TYPE}{TRANSFORM_REQUEST_ID}"),
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
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
fn catalog_only_identifiers_are_reserved() {
    for name in CATALOG_ONLY_AUTH {
        assert_eq!(name.rsplit('.').next(), Some("v1"));
    }
    assert_eq!(CATALOG_ONLY_GUARD.len(), 2);
    assert_eq!(CATALOG_ONLY_TRANSFORM.len(), 2);
}
