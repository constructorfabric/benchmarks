//! Unit tests for the GTS / plugin / permission identifier constants.

use super::*;
use uuid::Uuid;

#[test]
fn base_types_all_end_with_the_type_schema_terminator() {
    for base in BASE_TYPES {
        assert!(
            base.ends_with('~'),
            "base type `{base}` must end with the type-schema terminator `~`"
        );
    }
}

#[test]
fn base_type_set_is_exactly_the_documented_six() {
    assert_eq!(
        BASE_TYPES
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        vec![
            "gts.cf.core.oagw.upstream.v1~",
            "gts.cf.core.oagw.route.v1~",
            "gts.cf.core.oagw.auth_plugin.v1~",
            "gts.cf.core.oagw.guard_plugin.v1~",
            "gts.cf.core.oagw.transform_plugin.v1~",
            "gts.cf.core.oagw.proxy.v1~",
        ]
    );
}

#[test]
fn resource_ids_are_base_type_plus_uuid() {
    let id = Uuid::nil();
    assert_eq!(upstream_resource_id(id), format!("{UPSTREAM_BASE_TYPE}{id}"));
    assert_eq!(route_resource_id(id), format!("{ROUTE_BASE_TYPE}{id}"));
    assert_eq!(
        plugin_resource_id(GUARD_PLUGIN_BASE_TYPE, id),
        format!("gts.cf.core.oagw.guard_plugin.v1~{id}")
    );
}

#[test]
fn resource_id_examples_from_the_docs_round_trip() {
    // DESIGN/feature docs cite these two concrete instances.
    let upstream = "gts.cf.core.oagw.upstream.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let guard = "gts.cf.core.oagw.guard_plugin.v1~550e8400-e29b-41d4-a716-446655440000";
    assert_eq!(
        base_type_of(upstream),
        Some(UPSTREAM_BASE_TYPE),
        "an upstream instance resolves back to its base type"
    );
    assert_eq!(base_type_of(guard), Some(GUARD_PLUGIN_BASE_TYPE));
    assert_eq!(
        uuid_of(upstream),
        Some(Uuid::parse_str("7c9e6679-7425-40de-944b-e07fc1f90ae7").expect("valid uuid"))
    );
}

#[test]
fn builtin_and_catalog_only_identifier_sets_do_not_overlap() {
    for builtin in BUILTIN_PLUGIN_IDS {
        assert!(
            !CATALOG_ONLY_PLUGIN_IDS.contains(&builtin),
            "`{builtin}` cannot be both built-in and catalog-only"
        );
    }
}

#[test]
fn catalog_only_set_carries_the_six_documented_identifiers() {
    for (constant, short) in CATALOG_ONLY_PLUGIN_IDS
        .into_iter()
        .zip(["basic", "bearer", "timeout", "cors", "logging", "metrics"])
    {
        assert!(
            constant.contains(&format!("cf.core.oagw.{short}.v1")),
            "`{constant}` must name the `{short}` plugin"
        );
    }
}

#[test]
fn permission_identifiers_match_the_design_table() {
    assert_eq!(PERM_BIND, "oagw:upstream:bind");
    assert_eq!(PERM_OVERRIDE_AUTH, "oagw:upstream:override_auth");
    assert_eq!(PERM_OVERRIDE_RATE, "oagw:upstream:override_rate");
    assert_eq!(PERM_ADD_PLUGINS, "oagw:upstream:add_plugins");
}

#[test]
fn error_types_share_the_documented_prefix() {
    const PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";
    for err in [
        ERR_VALIDATION,
        ERR_MISSING_TARGET_HOST,
        ERR_INVALID_TARGET_HOST,
        ERR_UNKNOWN_TARGET_HOST,
        ERR_AUTH_FAILED,
        ERR_ROUTE_NOT_FOUND,
        ERR_PLUGIN_IN_USE,
        ERR_PAYLOAD_TOO_LARGE,
        ERR_RATE_LIMIT_EXCEEDED,
        ERR_SECRET_NOT_FOUND,
        ERR_PROTOCOL,
        ERR_DOWNSTREAM,
        ERR_STREAM_ABORTED,
        ERR_LINK_UNAVAILABLE,
        ERR_CIRCUIT_BREAKER_OPEN,
        ERR_PLUGIN_NOT_FOUND,
        ERR_CONNECTION_TIMEOUT,
        ERR_REQUEST_TIMEOUT,
        ERR_IDLE_TIMEOUT,
        ERR_CORS_ORIGIN_NOT_ALLOWED,
        ERR_CORS_METHOD_NOT_ALLOWED,
    ] {
        assert!(err.starts_with(PREFIX), "`{err}` must be under `{PREFIX}`");
    }
}

#[test]
fn uuid_of_rejects_named_plugin_identifiers() {
    assert_eq!(uuid_of(NOOP_AUTH_PLUGIN_ID), None);
    assert_eq!(base_type_of(NOOP_AUTH_PLUGIN_ID), Some(AUTH_PLUGIN_BASE_TYPE));
}
