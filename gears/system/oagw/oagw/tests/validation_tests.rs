//! Request-validation tests.
//!
//! Covers `cpt-cf-oagw-dod-request-validation` and
//! `cpt-cf-oagw-algo-request-validate`: one case per row of the FEATURE §3
//! family table, the single accumulated error that names every failing
//! property, the property that the detail never echoes a request body value,
//! the route replacement required-set narrowing, and the §1.5 route root
//! additions.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

// @cpt-dod:cpt-cf-oagw-dod-colocated-tests:p1

use std::sync::LazyLock;

use serde_json::{Value, json};

use oagw::control_plane::validation::{Validator, WriteKind};
use oagw::config::OagwConfig;
use oagw::gts::AUTH_PLUGIN_TYPE;
use oagw::{DomainError, ErrorKind};

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const SECRET: &str = "sk-live-abcdef0123456789";

/// The HTTPS-only posture the configuration defaults to.
static VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    Validator::compile(&OagwConfig::default()).expect("the shipped schemas compile")
});

/// The lifted posture that admits the `http` endpoint scheme literal.
static HTTP_ALLOWED: LazyLock<Validator> = LazyLock::new(|| {
    Validator::compile(&OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    })
    .expect("the shipped schemas compile")
});

/// Removes one root property from a body.
fn without(body: &Value, key: &str) -> Value {
    let mut body = body.clone();
    body.as_object_mut()
        .expect("the body is an object")
        .remove(key);
    body
}

/// A minimal valid upstream body.
fn upstream_body() -> Value {
    json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
        },
        "protocol": HTTP_PROTOCOL
    })
}

/// A minimal valid route body for a create.
fn route_body() -> Value {
    json!({
        "upstream_id": "00000000-0000-0000-0000-000000000001",
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "priority": 1
    })
}

/// The detail of a refused body, asserting it is one validation error.
fn refused(error: &DomainError) -> &str {
    assert_eq!(error.kind, ErrorKind::ValidationError, "{error}");
    &error.detail
}

/// Asserts a body is refused and the detail names `needle`.
fn refused_with(validator: &Validator, write: WriteKind, body: &Value, needle: &str) {
    let error = validator
        .validate_upstream(write, body)
        .expect_err("the body is refused");
    let detail = refused(&error);
    assert!(
        detail.contains(needle),
        "expected '{needle}' in the detail '{detail}'"
    );
}

/// Asserts an upstream body is refused and the detail names `needle`.
fn upstream_refused(body: &Value, needle: &str) {
    refused_with(&VALIDATOR, WriteKind::Create, body, needle);
}

/// Asserts a route body is refused and the detail names `needle`.
fn route_refused(body: &Value, needle: &str) {
    let error = VALIDATOR
        .validate_route(WriteKind::Create, body)
        .expect_err("the body is refused");
    let detail = refused(&error);
    assert!(
        detail.contains(needle),
        "expected '{needle}' in the detail '{detail}'"
    );
}

/// Asserts an upstream body is accepted.
fn accepted(validator: &Validator, body: &Value) {
    validator
        .validate_upstream(WriteKind::Create, body)
        .unwrap_or_else(|error| panic!("the body is refused: {error}"));
}

#[test]
fn a_valid_upstream_body_is_accepted() {
    accepted(&VALIDATOR, &upstream_body());
}

#[test]
fn a_valid_route_body_is_accepted() {
    VALIDATOR
        .validate_route(WriteKind::Create, &route_body())
        .unwrap_or_else(|error| panic!("the body is refused: {error}"));
}

#[test]
fn a_missing_required_property_is_named_per_resource_kind_and_method() {
    upstream_refused(&without(&upstream_body(), "server"), "server is required");
    upstream_refused(
        &without(&upstream_body(), "protocol"),
        "protocol is required",
    );
    route_refused(&without(&route_body(), "upstream_id"), "upstream_id is required");
    route_refused(&without(&route_body(), "match"), "match is required");
}

#[test]
fn an_unknown_property_is_named_at_the_root_and_in_each_sub_object() {
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "zzz": 1, "server": { "endpoints": [] }, "protocol": HTTP_PROTOCOL }),
            "unknown property 'zzz' at root",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }], "zzz": 1 }, "protocol": HTTP_PROTOCOL }),
            "unknown property 'zzz' at server",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "zzz": 1 }] }, "protocol": HTTP_PROTOCOL }),
            "unknown property 'zzz' at server.endpoints[0]",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] }, "protocol": HTTP_PROTOCOL, "headers": { "request": { "zzz": 1 } } }),
            "unknown property 'zzz' at headers.request",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] }, "protocol": HTTP_PROTOCOL, "rate_limit": { "sustained": { "rate": 1 }, "zzz": 1 } }),
            "unknown property 'zzz' at rate_limit",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] }, "protocol": HTTP_PROTOCOL, "cors": { "enabled": true, "zzz": 1 } }),
            "unknown property 'zzz' at cors",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] }, "protocol": HTTP_PROTOCOL, "match": { "http": { "methods": ["GET"], "path": "/v1" } } }),
            "unknown property 'match' at root",
        ),
    ];
    for (body, expected) in cases {
        upstream_refused(&body, expected);
    }

    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat", "zzz": 1 } },
            "priority": 1
        }),
        "unknown property 'zzz' at match.http",
    );
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
            "priority": 1,
            "grpc_match": {}
        }),
        "unknown property 'grpc_match' at root",
    );
}

#[test]
fn the_plugin_and_auth_objects_stay_open() {
    accepted(
        &VALIDATOR,
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "plugins": { "shipped": ["gts.cf.core.oagw.transform_plugin.v1~x.v1"] },
            "auth": { "type": AUTH_PLUGIN_TYPE, "config": { "header": "x-api-key" } }
        }),
    );
}

#[test]
fn an_endpoint_without_a_scheme_or_a_host_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL
        }),
        "server.endpoints[0].scheme is required",
    );
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https" }] },
            "protocol": HTTP_PROTOCOL
        }),
        "server.endpoints[0].host is required",
    );
}

#[test]
fn an_endpoint_host_that_is_neither_a_name_nor_an_ip_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "not a host" }] },
            "protocol": HTTP_PROTOCOL
        }),
        "server.endpoints[0].host",
    );
}

#[test]
fn an_endpoint_port_outside_the_range_is_refused() {
    for port in [0, 65_536] {
        upstream_refused(
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": port }] },
                "protocol": HTTP_PROTOCOL
            }),
            "server.endpoints[0].port",
        );
    }
}

#[test]
fn an_omitted_endpoint_port_defaults_to_443() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL
    });
    let validated = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect("the body is admitted");
    assert_eq!(validated.value.server.endpoints[0].port, Some(443));
}

#[test]
fn the_http_scheme_is_refused_while_the_posture_is_https_only() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL
    });
    upstream_refused(&body, "server.endpoints[0].scheme");
}

#[test]
fn the_http_scheme_is_admitted_when_the_posture_is_lifted() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "api.openai.com", "port": 80 }] },
        "protocol": HTTP_PROTOCOL
    });
    accepted(&HTTP_ALLOWED, &body);
}

#[test]
fn a_mixed_scheme_pool_is_refused() {
    let body = json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "api.openai.com", "port": 443 },
            { "scheme": "wss", "host": "api.openai.com", "port": 443 }
        ] },
        "protocol": HTTP_PROTOCOL
    });
    upstream_refused(&body, "server.endpoints[1].scheme");
}

#[test]
fn a_mixed_port_pool_is_refused() {
    let body = json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "api.openai.com", "port": 443 },
            { "scheme": "https", "host": "eu.openai.com", "port": 8443 }
        ] },
        "protocol": HTTP_PROTOCOL
    });
    upstream_refused(&body, "server.endpoints[1].port");
}

#[test]
fn a_protocol_outside_the_two_value_enum_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": "cf.core.oagw.http.v2"
        }),
        "protocol",
    );
}

#[test]
fn a_sharing_value_outside_the_enum_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sharing": "public", "sustained": { "rate": 1 } }
        }),
        "rate_limit.sharing",
    );
}

#[test]
fn a_match_with_both_or_neither_branch_is_refused() {
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"], "path": "/v1" }, "grpc": { "service": "s", "method": "m" } },
            "priority": 1
        }),
        "match",
    );
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": {},
            "priority": 1
        }),
        "match",
    );
}

#[test]
fn an_http_match_without_methods_or_path_is_refused() {
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "path": "/v1" } },
            "priority": 1
        }),
        "match.http.methods is required",
    );
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"] } },
            "priority": 1
        }),
        "match.http.path is required",
    );
}

#[test]
fn an_http_method_outside_the_enum_is_refused() {
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["TRACE"], "path": "/v1" } },
            "priority": 1
        }),
        "match.http.methods",
    );
}

#[test]
fn a_grpc_match_without_service_or_method_is_refused() {
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "grpc": { "method": "GetUser" } },
            "priority": 1
        }),
        "match.grpc.service is required",
    );
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "grpc": { "service": "foo.v1.UserService" } },
            "priority": 1
        }),
        "match.grpc.method is required",
    );
}

#[test]
fn a_route_without_a_priority_is_refused() {
    route_refused(
        &json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }),
        "priority",
    );
}

#[test]
fn a_rate_limit_without_sustained_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sharing": "private" }
        }),
        "rate_limit.sustained",
    );
}

#[test]
fn a_sustained_rate_below_one_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sustained": { "rate": 0 } }
        }),
        "rate_limit.sustained.rate",
    );
}

#[test]
fn a_window_outside_the_enum_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sustained": { "rate": 1, "window": "week" } }
        }),
        "rate_limit.sustained.window",
    );
}

#[test]
fn a_burst_capacity_below_one_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sustained": { "rate": 1 }, "burst": { "capacity": 0 } }
        }),
        "rate_limit.burst.capacity",
    );
}

#[test]
fn a_cost_below_one_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "rate_limit": { "sustained": { "rate": 1 }, "cost": 0 }
        }),
        "rate_limit.cost",
    );
}

#[test]
fn a_cors_object_without_enabled_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "cors": { "allowed_origins": ["https://console.vendor.com"] }
        }),
        "cors.enabled",
    );
}

#[test]
fn credentials_with_a_wildcard_origin_are_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "cors": { "enabled": true, "allow_credentials": true, "allowed_origins": ["*"] }
        }),
        "cors.allowed_origins",
    );
}

#[test]
fn an_origin_without_a_uri_scheme_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "cors": { "enabled": true, "allowed_origins": ["console.vendor.com"] }
        }),
        "cors.allowed_origins[0]",
    );
}

#[test]
fn an_allowed_method_outside_the_enum_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "cors": { "enabled": true, "allowed_methods": ["TRACE"] }
        }),
        "cors.allowed_methods[0]",
    );
}

#[test]
fn a_route_level_cors_object_is_validated_with_the_upstream_shape() {
    let refused = json!({
        "upstream_id": "00000000-0000-0000-0000-000000000001",
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 1,
        "cors": { "allowed_origins": ["console.vendor.com"] }
    });
    route_refused(&refused, "cors.enabled");

    let admitted = json!({
        "upstream_id": "00000000-0000-0000-0000-000000000001",
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 1,
        "cors": { "enabled": true, "allowed_origins": ["https://console.vendor.com"] }
    });
    VALIDATOR
        .validate_route(WriteKind::Create, &admitted)
        .unwrap_or_else(|error| panic!("the body is refused: {error}"));
}

#[test]
fn a_tag_outside_the_pattern_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "tags": ["Bad Tag"]
        }),
        "tags[0]",
    );
}

#[test]
fn a_credential_reference_without_the_cred_scheme_is_refused() {
    upstream_refused(
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "auth": { "config": { "secret_ref": "vault://prod/key" } }
        }),
        "auth.config.secret_ref",
    );
}

#[test]
fn a_credential_reference_with_the_cred_scheme_is_admitted() {
    accepted(
        &VALIDATOR,
        &json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": HTTP_PROTOCOL,
            "auth": { "config": { "secret_ref": "cred://prod/key" } }
        }),
    );
}

#[test]
fn two_defects_produce_one_error_naming_both_properties() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": "cf.core.oagw.http.v2",
        "tags": ["Bad Tag"]
    });
    let error = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect_err("the body is refused");
    let detail = refused(&error);
    assert!(detail.contains("protocol"), "{detail}");
    assert!(detail.contains("tags[0]"), "{detail}");
    assert!(detail.contains(','), "{detail}");
}

#[test]
fn the_detail_never_echoes_a_request_body_value() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": SECRET,
        "auth": { "config": { "api_key": SECRET } },
        "unexpected_property": SECRET
    });
    let error = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect_err("the body is refused");
    let detail = refused(&error);
    assert!(!detail.contains(SECRET), "the detail echoed a value: {detail}");
    assert!(detail.contains("protocol"), "{detail}");
    assert!(
        detail.contains("unknown property 'unexpected_property' at root"),
        "{detail}"
    );
}

#[test]
fn a_body_that_is_not_an_object_is_refused() {
    upstream_refused(&json!([1, 2, 3]), "root");
}

#[test]
fn a_body_that_cannot_deserialize_names_a_property() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL,
        "enabled": "yes"
    });
    let error = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect_err("the body is refused");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(!error.detail.contains(SECRET), "{error}");
}

#[test]
fn a_route_replacement_does_not_require_upstream_id() {
    let body = json!({
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "priority": 2
    });
    let validated = VALIDATOR
        .validate_route(WriteKind::Replacement, &body)
        .expect("the replacement is admitted");
    assert!(validated.value.upstream_id.is_nil(), "no upstream reference");
    assert_eq!(validated.value.priority, Some(2));
}

#[test]
fn a_route_replacement_rejects_a_supplied_upstream_id() {
    let body = json!({
        "upstream_id": "00000000-0000-0000-0000-000000000001",
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "priority": 2
    });
    let error = VALIDATOR
        .validate_route(WriteKind::Replacement, &body)
        .expect_err("the replacement is refused");
    assert!(
        refused(&error).contains("unknown property 'upstream_id' at root"),
        "{error}"
    );
}

#[test]
fn a_route_replacement_still_requires_match() {
    let body = json!({ "priority": 2 });
    let error = VALIDATOR
        .validate_route(WriteKind::Replacement, &body)
        .expect_err("the replacement is refused");
    let detail = refused(&error);
    assert!(detail.contains("match is required"), "{detail}");
    assert!(
        !detail.contains("upstream_id is required"),
        "the replacement required set was not narrowed: {detail}"
    );
}

#[test]
fn a_route_body_may_carry_priority_enabled_and_cors() {
    let body = json!({
        "upstream_id": "00000000-0000-0000-0000-000000000001",
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "priority": 3,
        "enabled": false,
        "cors": { "enabled": true, "allowed_origins": ["https://console.vendor.com"] }
    });
    let validated = VALIDATOR
        .validate_route(WriteKind::Create, &body)
        .expect("the §1.5 route properties are admitted");
    assert_eq!(validated.value.enabled, Some(false));
    assert_eq!(validated.value.priority, Some(3));
    assert!(validated.value.cors.is_some());
}

#[test]
fn a_create_rejects_a_supplied_id_and_tenant_id() {
    let body = json!({
        "id": "00000000-0000-0000-0000-000000000009",
        "tenant_id": "00000000-0000-0000-0000-000000000002",
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL
    });
    let error = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect_err("the create is refused");
    let detail = refused(&error);
    assert!(detail.contains("id"), "{detail}");
    assert!(detail.contains("tenant_id"), "{detail}");
}

#[test]
fn a_replacement_may_carry_its_own_id() {
    let body = json!({
        "id": "00000000-0000-0000-0000-000000000009",
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL
    });
    let validated = VALIDATOR
        .validate_upstream(WriteKind::Replacement, &body)
        .expect("the replacement carries its own identifier");
    assert_eq!(
        validated.stated_id,
        Some(uuid::Uuid::from_u128(9)),
        "the stated identifier is carried for the diff"
    );
}

#[test]
fn a_replacement_defaults_enabled_when_the_body_omits_it() {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": HTTP_PROTOCOL
    });
    let created = VALIDATOR
        .validate_upstream(WriteKind::Create, &body)
        .expect("created");
    assert!(created.value.enabled, "a create defaults enabled to true");

    let replaced = VALIDATOR
        .validate_route(WriteKind::Replacement, &json!({
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "priority": 1
        }))
        .expect("replaced");
    assert_eq!(
        replaced.value.enabled, None,
        "a replacement carries the stored value forward"
    );
}
