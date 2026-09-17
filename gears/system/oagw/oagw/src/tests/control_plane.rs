//! Control-plane tests: upstream/route CRUD, tenant scoping, payload
//! validation, alias derivation, and the plugin catalogue.

use axum::http::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use std::sync::Arc;

use super::{
    ERROR_TYPE_BASE, Gateway, assert_problem, assert_problem_body, body_json, delete, gateway,
    get_json, post_json, problem_field, put_json, request, route_json, send, status_of,
    upstream_json,
};
use crate::domain::alias::normalize;
use crate::domain::error::ErrorKind;
use crate::domain::model::{
    CustomPlugin, GrpcMatch, Route, RouteMatch, guard_plugin_ids, transform_plugin_ids,
};
use crate::domain::validation::validate_route;
use crate::infra::plugin::catalogue::{BUILTIN_PLUGIN_IDS, plugin_kind};

/// The tenant most control-plane tests call from.
fn tenant_a() -> Uuid {
    Uuid::from_u128(0xa001)
}

/// A second tenant, used to assert scoping.
fn tenant_b() -> Uuid {
    Uuid::from_u128(0xb002)
}

/// Create an upstream and return its stored JSON body.
async fn create_upstream(gateway: &Gateway, host: &str, port: u16, alias: &str) -> Value {
    let mut response = post_json(
        gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        upstream_json(host, port, alias),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201, "upstream created");
    body_json(&mut response).await
}

// ---------------------------------------------------------------------------
// Upstream CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_crud_round_trip() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 8443, "api.local").await;

    assert!(!created["id"].as_str().unwrap_or("").is_empty());
    assert_eq!(created["tenant_id"], json!(tenant_a()));
    assert_eq!(created["alias"], json!("api.local"));
    assert_eq!(created["enabled"], json!(true));
    assert_eq!(
        created["server"]["endpoints"][0]["scheme"],
        json!("http"),
        "the endpoint scheme field accepts http"
    );
    assert_eq!(created["server"]["endpoints"][0]["port"], json!(8443));

    let id = created["id"].as_str().expect("id").to_owned();
    let mut fetched = get_json(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_a()).await;
    assert_eq!(status_of(&fetched).as_u16(), 200);
    let fetched_body = body_json(&mut fetched).await;
    assert_eq!(
        fetched_body["id"], created["id"],
        "get returns the upstream"
    );

    let mut listed = get_json(&gateway, "/oagw/v1/upstreams", tenant_a()).await;
    let listed_body = body_json(&mut listed).await;
    assert_eq!(listed_body.as_array().map(Vec::len), Some(1));

    // Replace: full replacement, id and tenant are preserved.
    let mut replacement = created.clone();
    replacement["enabled"] = json!(false);
    replacement["tags"] = json!(["team-a"]);
    let mut replaced = put_json(
        &gateway,
        &format!("/oagw/v1/upstreams/{id}"),
        tenant_a(),
        replacement,
    )
    .await;
    assert_eq!(status_of(&replaced).as_u16(), 200);
    let replaced_body = body_json(&mut replaced).await;
    assert_eq!(replaced_body["alias"], json!("api.local"));
    assert_eq!(replaced_body["enabled"], json!(false));
    assert_eq!(replaced_body["tags"], json!(["team-a"]));

    let deleted = delete(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_a()).await;
    assert_eq!(status_of(&deleted).as_u16(), 204, "delete is 204");
    let after = get_json(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_a()).await;
    assert_eq!(status_of(&after).as_u16(), 404, "get after delete is 404");
}

#[tokio::test]
async fn upstreams_are_scoped_per_tenant() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9001, "shared.local").await;
    let id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");

    // Another tenant never sees the upstream.
    let foreign = get_json(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_b()).await;
    assert_eq!(status_of(&foreign).as_u16(), 404, "peers are invisible");
    assert_problem(&foreign, ErrorKind::RouteNotFound.gts_fragment(), 404);

    let mut listed = get_json(&gateway, "/oagw/v1/upstreams", tenant_b()).await;
    let listed_body = body_json(&mut listed).await;
    assert_eq!(listed_body.as_array().map(Vec::len), Some(0), "list empty");

    let deleted = delete(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_b()).await;
    assert_eq!(status_of(&deleted).as_u16(), 404, "foreign delete is 404");

    // Aliases are unique per tenant, not globally: tenant B may reuse it.
    let own = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_b(),
        upstream_json("127.0.0.1", 9002, "shared.local"),
    )
    .await;
    assert_eq!(status_of(&own).as_u16(), 201, "alias unique per tenant");

    // A second upstream with the same alias in the same tenant is rejected:
    // the alias is claimed, so the request is well formed but inapplicable.
    let mut duplicate = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        upstream_json("127.0.0.1", 9003, "shared.local"),
    )
    .await;
    assert_eq!(
        status_of(&duplicate).as_u16(),
        409,
        "duplicate alias in one tenant is a conflict"
    );
    assert_problem(&duplicate, ErrorKind::Conflict.gts_fragment(), 409);
    let body = body_json(&mut duplicate).await;
    assert_eq!(body["context"]["alias"], json!("shared.local"));
}

#[tokio::test]
async fn deleting_an_upstream_removes_its_routes() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9004, "cascade.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");
    let mut route = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(upstream, "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&route).as_u16(), 201);
    let route_body = body_json(&mut route).await;
    let route_id = Uuid::parse_str(route_body["id"].as_str().expect("id")).expect("uuid id");

    let deleted = delete(
        &gateway,
        &format!("/oagw/v1/upstreams/{upstream}"),
        tenant_a(),
    )
    .await;
    assert_eq!(status_of(&deleted).as_u16(), 204);
    let orphan = get_json(&gateway, &format!("/oagw/v1/routes/{route_id}"), tenant_a()).await;
    assert_eq!(status_of(&orphan).as_u16(), 404, "route cascaded away");
}

// ---------------------------------------------------------------------------
// Alias rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hostname_alias_is_derived_and_normalized() {
    let gateway = gateway();
    // The schema pattern for an alias (`valid_alias`) is applied to the value
    // the caller sent, before `resolve_alias` normalizes it, so an alias that
    // is not already canonical is rejected rather than cleaned up.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "API.Vendor.COM.",
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }]
            }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");

    // The normalizer itself lowercases and strips the trailing dot; only a
    // caller that sends an already-canonical alias reaches the API.
    assert_eq!(normalize("API.Vendor.COM. "), "api.vendor.com");
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "api.vendor.com",
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }]
            }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201);
    let body = body_json(&mut response).await;
    assert_eq!(body["alias"], json!("api.vendor.com"));
}

#[tokio::test]
async fn non_standard_port_is_part_of_the_alias() {
    let gateway = gateway();
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "api.vendor.com:8443",
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": 8443 }]
            }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201);
    let body = body_json(&mut response).await;
    assert_eq!(body["alias"], json!("api.vendor.com:8443"));
}

#[tokio::test]
async fn multi_endpoint_alias_is_the_common_suffix() {
    let gateway = gateway();
    let pool = |alias: &str| {
        json!({
            "alias": alias,
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "us.vendor.com" },
                    { "scheme": "https", "host": "eu.vendor.com" }
                ]
            }
        })
    };

    // The suffix alias is mandatory: an endpoint host is not accepted.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        pool("us.vendor.com"),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("vendor.com"),
        "the problem names the derived alias, got: {body}"
    );

    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        pool("vendor.com"),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201);
    let body = body_json(&mut response).await;
    assert_eq!(body["alias"], json!("vendor.com"));
}

#[tokio::test]
async fn bare_public_suffix_and_ip_pools_require_an_explicit_alias() {
    let gateway = gateway();
    // Two hosts under a bare public suffix: nothing is derivable.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "",
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "foo.co.uk" },
                    { "scheme": "https", "host": "bar.co.uk" }
                ]
            }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("explicit alias"),
        "detail must ask for an explicit alias, got: {body}"
    );

    // A single IP endpoint is never derivable either.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9100 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");

    // With an explicit alias the IP pool is accepted.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        upstream_json("127.0.0.1", 9101, "ip-pool.local"),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201);
    let body = body_json(&mut response).await;
    assert_eq!(body["alias"], json!("ip-pool.local"));
}

#[tokio::test]
async fn malformed_alias_is_rejected() {
    let gateway = gateway();
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "-not-an-alias",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9102 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_endpoints_are_rejected_naming_the_field() {
    let gateway = gateway();

    // Empty endpoint pool.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({ "alias": "empty.local", "server": { "endpoints": [] } }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "server.endpoints");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("endpoint")
    );

    // Malformed host label.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "bad-host.local",
            "server": { "endpoints": [{ "scheme": "http", "host": "-bad-host-", "port": 80 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "server.endpoints[].host");

    // Port zero.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "zero-port.local",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 0 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "server.endpoints[].port");
}

#[tokio::test]
async fn invalid_scheme_breaks_deserialization_before_validation() {
    let gateway = gateway();
    let response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "scheme.local",
            "server": { "endpoints": [{ "scheme": "ftp", "host": "127.0.0.1", "port": 9103 }] }
        }),
    )
    .await;
    // Documented implementation behaviour: the payload never reaches the
    // handler, so axum's own JSON rejection (422 for a body that parses but
    // does not deserialize) answers instead of an OAGW problem.
    assert_eq!(status_of(&response).as_u16(), 422);
    assert_ne!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        crate::domain::error::PROBLEM_JSON,
        "an undeclared scheme value never reaches the OAGW validator"
    );
}

#[tokio::test]
async fn unsupported_protocol_tags_and_cors_conflicts_are_rejected() {
    let gateway = gateway();

    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "proto.local",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.mqtt.v1",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9104 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "protocol");

    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "tags.local",
            "tags": ["Not-A-Tag"],
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9105 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "tags");

    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "cors.local",
            "cors": {
                "enabled": true,
                "allowed_origins": ["*"],
                "allow_credentials": true
            },
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9106 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "cors.allowed_origins");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("allow_credentials"),
        "the problem explains the credentials restriction, got: {body}"
    );

    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "rate.local",
            "rate_limit": {
                "sustained": { "rate": 0, "window": "second" },
                "scope": "tenant"
            },
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9107 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "rate_limit.sustained.rate");
}

#[tokio::test]
async fn catalogue_only_plugin_kinds_are_rejected_at_validation() {
    let gateway = gateway();
    let mut catalogue = get_json(&gateway, "/oagw/v1/plugins", tenant_a()).await;
    assert_eq!(status_of(&catalogue).as_u16(), 200);
    let catalogue_body = body_json(&mut catalogue).await;
    assert_eq!(
        catalogue_body.as_array().map(Vec::len),
        Some(BUILTIN_PLUGIN_IDS.len()),
        "the built-in catalogue is fully advertised"
    );

    // Auth catalogue-only identifiers have no backing implementation.
    for identifier in [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    ] {
        let mut response = post_json(
            &gateway,
            "/oagw/v1/upstreams",
            tenant_a(),
            json!({
                "alias": "auth-catalogue.local",
                "auth": { "type": identifier, "config": {} },
                "server": {
                    "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9108 }]
                }
            }),
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 400, "{identifier}");
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(problem_field(&body), "auth.type");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains(identifier),
            "the problem names the offending plugin, got: {body}"
        );
    }

    // Guard and transform catalogue-only identifiers fail the chain check.
    for identifier in [
        guard_plugin_ids::TIMEOUT,
        guard_plugin_ids::CORS,
        transform_plugin_ids::LOGGING,
        transform_plugin_ids::METRICS,
    ] {
        let mut response = post_json(
            &gateway,
            "/oagw/v1/upstreams",
            tenant_a(),
            json!({
                "alias": "catalogue.local",
                "plugins": { "items": [identifier] },
                "server": {
                    "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9109 }]
                }
            }),
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 400, "{identifier}");
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(problem_field(&body), "plugins.items[]");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains(identifier),
            "the problem names the offending plugin, got: {body}"
        );
    }
}

#[tokio::test]
async fn unknown_plugin_reference_is_rejected() {
    let gateway = gateway();
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "plugin.local",
            "plugins": {
                "items": [{ "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~nope", "config": {} }]
            },
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9110 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "plugins.items[]");
}

#[tokio::test]
async fn route_validation_rejects_bad_match_rules() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9111, "routes.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");

    // Unknown upstream.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(Uuid::new_v4(), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400, "unknown upstream");
    let body = body_json(&mut response).await;
    assert_eq!(
        ErrorKind::Validation.gts_fragment(),
        body["type"]
            .as_str()
            .and_then(|value| value.split('~').next_back())
            .unwrap_or_default()
    );

    // Nil upstream id.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(Uuid::nil(), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "upstream_id");

    // Both match strategies at once.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        json!({
            "upstream_id": upstream,
            "match": {
                "http": { "methods": ["GET"], "path": "/v1" },
                "grpc": { "service": "svc.S", "method": "M" }
            }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "match");

    // No method allowlist.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": [], "path": "/v1" } }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "match.http.methods");

    // OPTIONS is reserved for the CORS layer, but the wire schema has no
    // OPTIONS variant at all, so such a payload is refused by the
    // deserializer before the validator's own guard can run.
    let response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["OPTIONS"], "path": "/v1" } }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 422);
    assert_ne!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        crate::domain::error::PROBLEM_JSON,
        "an unknown method never reaches the OAGW validator"
    );
    // The validator's remaining rules are reachable with an in-memory route.
    let route = Route {
        upstream_id: upstream,
        match_rule: RouteMatch {
            grpc: Some(GrpcMatch::default()),
            ..RouteMatch::default()
        },
        ..Route::default()
    };
    let error = validate_route(
        &route,
        gateway.control.guards(),
        gateway.control.transforms(),
        &|_| true,
    )
    .expect_err("a gRPC match needs a service and a method");
    assert_eq!(
        error.kind().gts_fragment(),
        ErrorKind::Validation.gts_fragment()
    );
    assert!(error.detail().contains("match.grpc"), "{}", error.detail());

    // Path must start with a slash.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "v1" } }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "match.http.path");
}

#[tokio::test]
async fn route_crud_round_trip_and_immutability() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9112, "crud.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");

    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(upstream, "/v1", &["POST"]),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 201, "route created");
    let route = body_json(&mut response).await;
    let id = Uuid::parse_str(route["id"].as_str().expect("id")).expect("uuid id");
    assert_eq!(route["tenant_id"], json!(tenant_a()));
    assert_eq!(route["match"]["http"]["path"], json!("/v1"));

    let mut listed = get_json(&gateway, "/oagw/v1/routes", tenant_a()).await;
    let listed_body = body_json(&mut listed).await;
    assert_eq!(listed_body.as_array().map(Vec::len), Some(1));

    // Replace: path changes, upstream_id is immutable.
    let mut replacement = route.clone();
    replacement["match"]["http"]["path"] = json!("/v2");
    let mut replaced = put_json(
        &gateway,
        &format!("/oagw/v1/routes/{id}"),
        tenant_a(),
        replacement,
    )
    .await;
    assert_eq!(status_of(&replaced).as_u16(), 200);
    let replaced_body = body_json(&mut replaced).await;
    assert_eq!(replaced_body["match"]["http"]["path"], json!("/v2"));

    let mut moved = route.clone();
    moved["upstream_id"] = json!(Uuid::new_v4());
    let mut response = put_json(
        &gateway,
        &format!("/oagw/v1/routes/{id}"),
        tenant_a(),
        moved,
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400, "upstream_id immutable");
    let body = body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("upstream_id is immutable"),
        "the store names the immutable member, got: {body}"
    );

    // A second route claiming the same path and method is a conflict: the match
    // rule is unique per upstream (`DESIGN.md` data model).
    let mut response = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(upstream, "/v2", &["POST"]),
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        409,
        "match-rule uniqueness is enforced on create"
    );
    let body = body_json(&mut response).await;
    assert_eq!(
        body["type"],
        json!(format!(
            "{ERROR_TYPE_BASE}{}",
            ErrorKind::Conflict.gts_fragment()
        ))
    );
    assert_eq!(body["error_code"], json!("CONFLICT"));

    let deleted = delete(&gateway, &format!("/oagw/v1/routes/{id}"), tenant_a()).await;
    assert_eq!(status_of(&deleted).as_u16(), 204);
    let missing = get_json(&gateway, &format!("/oagw/v1/routes/{id}"), tenant_a()).await;
    assert_eq!(status_of(&missing).as_u16(), 404);
}

/// Two enabled routes of one upstream may share a path and method only when
/// their priorities differ (`DESIGN.md` §CRUD): an identical
/// `(path, priority, method)` claim is a `409`, while the same path at another
/// priority is a separate route the matcher ranks by priority.
#[tokio::test]
async fn a_shared_path_and_priority_is_a_conflict_but_a_distinct_priority_is_kept() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9113, "priority.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");

    let mut first = route_json(upstream, "/v1", &["POST"]);
    first["priority"] = json!(4);
    let mut response = post_json(&gateway, "/oagw/v1/routes", tenant_a(), first).await;
    assert_eq!(
        status_of(&response).as_u16(),
        201,
        "the first claim is stored"
    );
    let stored = body_json(&mut response).await;

    let mut duplicate = route_json(upstream, "/v1", &["POST"]);
    duplicate["priority"] = json!(4);
    let mut response = post_json(&gateway, "/oagw/v1/routes", tenant_a(), duplicate).await;
    assert_eq!(
        status_of(&response).as_u16(),
        409,
        "same path, priority, and method is a conflict on create"
    );
    assert_problem(&response, ErrorKind::Conflict.gts_fragment(), 409);
    let body = body_json(&mut response).await;
    assert_eq!(body["error_code"], json!("CONFLICT"));
    assert_eq!(
        body["context"]["priority"],
        json!(4),
        "the colliding priority is named"
    );
    assert_eq!(body["context"]["path"], json!("/v1"));

    // Another priority on the same path and method is a different route.
    let mut lower = route_json(upstream, "/v1", &["POST"]);
    lower["priority"] = json!(1);
    let mut response = post_json(&gateway, "/oagw/v1/routes", tenant_a(), lower).await;
    assert_eq!(
        status_of(&response).as_u16(),
        201,
        "the same path at another priority coexists"
    );
    let lower_id = Uuid::parse_str(body_json(&mut response).await["id"].as_str().expect("id"))
        .expect("uuid id");

    // Replacing it into the taken priority is refused the same way.
    let mut raised = stored.clone();
    raised["id"] = json!(lower_id);
    raised["priority"] = json!(4);
    let mut response = put_json(
        &gateway,
        &format!("/oagw/v1/routes/{lower_id}"),
        tenant_a(),
        raised,
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        409,
        "the same claim is refused on replace"
    );
    let _ = body_json(&mut response).await;

    // A disabled route holds no claim: the matcher never serves it.
    let mut disabled = route_json(upstream, "/v1", &["POST"]);
    disabled["priority"] = json!(4);
    disabled["enabled"] = json!(false);
    let mut response = post_json(&gateway, "/oagw/v1/routes", tenant_a(), disabled).await;
    assert_eq!(
        status_of(&response).as_u16(),
        201,
        "a disabled route does not collide"
    );
    let _ = body_json(&mut response).await;
}

#[tokio::test]
async fn unknown_resource_reads_and_deletes_are_404_problems() {
    let gateway = gateway();
    let missing = Uuid::new_v4();
    for (method, path) in [
        (Method::GET, format!("/oagw/v1/upstreams/{missing}")),
        (Method::DELETE, format!("/oagw/v1/upstreams/{missing}")),
        (Method::GET, format!("/oagw/v1/routes/{missing}")),
        (Method::DELETE, format!("/oagw/v1/routes/{missing}")),
    ] {
        let mut response = send(&gateway.router, request(method.clone(), &path, tenant_a())).await;
        assert_eq!(status_of(&response).as_u16(), 404, "{method} {path}");
        assert_problem_body(
            &mut response,
            ErrorKind::RouteNotFound.gts_fragment(),
            404,
            &path,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Plugin catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_catalogue_lists_builtins_and_tenant_plugins() {
    let gateway = gateway();
    let plugin = CustomPlugin {
        tenant_id: tenant_a(),
        plugin_type: "transform".to_owned(),
        name: "pii-redactor".to_owned(),
        source_code: "def on_response(ctx):\n    return ctx.next()\n".to_owned(),
        ..CustomPlugin::default()
    };
    let custom = gateway
        .control
        .insert_plugin(plugin)
        .expect("an unreferenced name is stored");
    let custom_id = custom.id;

    let mut response = get_json(&gateway, "/oagw/v1/plugins", tenant_a()).await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let body = body_json(&mut response).await;
    let entries = body.as_array().expect("catalogue is a list");

    assert_eq!(
        entries.len(),
        BUILTIN_PLUGIN_IDS.len() + 1,
        "builtins plus the tenant-defined plugin"
    );
    let custom_entry = entries
        .iter()
        .find(|entry| entry["id"] == json!(custom_id))
        .expect("tenant plugin is catalogued");
    assert_eq!(custom_entry["name"], json!("pii-redactor"));
    assert_eq!(custom_entry["plugin_type"], json!("transform"));

    // Built-ins come first, in `BUILTIN_PLUGIN_IDS` order, and are named
    // rather than stored: their `id` is the nil UUID and they are never
    // garbage collected.
    for (index, identifier) in BUILTIN_PLUGIN_IDS.iter().enumerate() {
        let entry = &entries[index];
        assert_eq!(entry["id"], json!(Uuid::nil()), "{identifier}");
        assert_eq!(
            entry["plugin_type"],
            json!(plugin_kind(identifier)),
            "{identifier}"
        );
        assert!(
            entry["gc_eligible"].is_boolean(),
            "catalogued definitions are complete CustomPlugin values"
        );
    }
    // Documented short names (`PRD.md` "Built-in Plugins"): the GTS instance's
    // trailing version segment is not part of the name, so the catalogue can
    // tell `noop` from `apikey` and each entry carries the plugin kind its
    // identifier declares.
    let documented = [
        ("noop", "auth"),
        ("apikey", "auth"),
        ("oauth2_client_cred", "auth"),
        ("oauth2_client_cred_basic", "auth"),
        ("required_headers", "guard"),
        ("request_id", "transform"),
    ];
    for (name, kind) in documented {
        let entry = entries
            .iter()
            .find(|entry| entry["name"] == json!(name))
            .unwrap_or_else(|| panic!("the catalogue advertises the built-in `{name}`"));
        assert_eq!(entry["plugin_type"], json!(kind), "{name}");
        assert_eq!(entry["id"], json!(Uuid::nil()), "{name}");
        assert_eq!(entry["tenant_id"], json!(Uuid::nil()), "{name}");
    }
    // Every built-in is named, and no two of them share a name: otherwise the
    // catalogue could not distinguish one built-in from another.
    let mut names: Vec<&str> = entries
        .iter()
        .take(BUILTIN_PLUGIN_IDS.len())
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert_eq!(
        names.len(),
        BUILTIN_PLUGIN_IDS.len(),
        "every built-in is named"
    );
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names.len(),
        BUILTIN_PLUGIN_IDS.len(),
        "built-in short names are distinct"
    );

    // A different tenant does not see the custom plugin.
    let mut foreign = get_json(&gateway, "/oagw/v1/plugins", tenant_b()).await;
    let foreign_body = body_json(&mut foreign).await;
    assert_eq!(
        foreign_body.as_array().map(Vec::len),
        Some(BUILTIN_PLUGIN_IDS.len()),
        "tenant-defined plugins are scoped"
    );
}

#[tokio::test]
async fn plugin_in_use_cannot_be_deleted() {
    let gateway = gateway();
    let plugin = CustomPlugin {
        tenant_id: tenant_a(),
        plugin_type: "transform".to_owned(),
        name: "order-guard".to_owned(),
        ..CustomPlugin::default()
    };
    let id = gateway
        .control
        .insert_plugin(plugin)
        .expect("an unreferenced name is stored")
        .id;

    // A referenced plugin is not deletable. The reference is written through the
    // store: the management API refuses a custom-plugin binding (this build has
    // no interpreter for one), but a stored reference still blocks deletion.
    let upstream = crate::domain::model::Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant_a(),
        alias: "in-use.local".to_owned(),
        plugins: crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![crate::domain::model::PluginBindingDto::Ref(id.to_string())],
        },
        ..crate::domain::model::Upstream::default()
    };
    gateway
        .store
        .insert_upstream(upstream)
        .expect("the referencing upstream is stored");

    // ADR 0001 "Plugin Deletion Behavior": a referenced plugin is not
    // deletable; the store classifies the conflict as `PluginInUse`.
    let error = gateway
        .store
        .delete_plugin(tenant_a(), id)
        .expect_err("a referenced plugin must not be deletable");
    assert_eq!(error.kind(), ErrorKind::PluginInUse);
    assert_eq!(error.kind().status().as_u16(), 409);
    assert_eq!(error.kind().gts_fragment(), "cf.oagw.plugin.in_use.v1");
    assert!(
        error.detail().contains("referenced"),
        "detail must explain the conflict: {}",
        error.detail()
    );

    // Unlinked plugins delete cleanly.
    let plugin = CustomPlugin {
        tenant_id: tenant_a(),
        name: "orphan".to_owned(),
        ..CustomPlugin::default()
    };
    let orphan = gateway
        .control
        .insert_plugin(plugin)
        .expect("an unreferenced name is stored")
        .id;
    let deleted = gateway
        .store
        .delete_plugin(tenant_a(), orphan)
        .expect("unlinked plugin deletes");
    assert_eq!(deleted.name, "orphan");
    assert!(
        gateway.control.get_plugin(tenant_a(), orphan).is_none(),
        "deleted plugin is gone"
    );
}

// ---------------------------------------------------------------------------
// Plugin management
// ---------------------------------------------------------------------------

/// A custom-plugin payload the management API accepts.
fn plugin_json(name: &str) -> Value {
    json!({
        "plugin_type": "transform",
        "name": name,
        "source_code": "def on_request(ctx):\n    return ctx.next()\n",
    })
}

#[tokio::test]
async fn plugin_crud_round_trip() {
    let gateway = gateway();

    // Create.
    let mut created = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        plugin_json("pii-redactor"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "plugin created");
    let plugin = body_json(&mut created).await;
    let id = Uuid::parse_str(plugin["id"].as_str().expect("id")).expect("uuid id");
    assert_eq!(plugin["tenant_id"], json!(tenant_a()));
    assert_eq!(plugin["plugin_type"], json!("transform"));
    assert_eq!(plugin["name"], json!("pii-redactor"));

    // Get.
    let mut fetched = get_json(&gateway, &format!("/oagw/v1/plugins/{id}"), tenant_a()).await;
    assert_eq!(status_of(&fetched).as_u16(), 200);
    assert_eq!(body_json(&mut fetched).await["name"], json!("pii-redactor"));

    // Source.
    let mut source = get_json(
        &gateway,
        &format!("/oagw/v1/plugins/{id}/source"),
        tenant_a(),
    )
    .await;
    assert_eq!(status_of(&source).as_u16(), 200);
    let source_body = body_json(&mut source).await;
    assert_eq!(
        source_body["id"],
        json!(format!("{}{id}", crate::domain::model::PLUGIN_GTS_BASE)),
        "the source names the plugin by its GTS identifier"
    );
    assert_eq!(source_body["plugin_type"], json!("transform"));
    assert!(
        source_body["source_code"]
            .as_str()
            .is_some_and(|code| code.contains("on_request")),
        "the stored Starlark source is returned verbatim"
    );

    // Catalogue now lists it too.
    let mut listed = get_json(&gateway, "/oagw/v1/plugins", tenant_a()).await;
    let entries = body_json(&mut listed).await;
    assert!(
        entries
            .as_array()
            .expect("catalogue is a list")
            .iter()
            .any(|entry| entry["id"] == json!(id)),
        "the created plugin is catalogued"
    );

    // Delete.
    let deleted = delete(&gateway, &format!("/oagw/v1/plugins/{id}"), tenant_a()).await;
    assert_eq!(status_of(&deleted).as_u16(), 204, "delete is 204");
    let after = get_json(&gateway, &format!("/oagw/v1/plugins/{id}"), tenant_a()).await;
    assert_eq!(status_of(&after).as_u16(), 404, "get after delete is 404");
}

/// A custom plugin's name is unique within its tenant (`DESIGN.md` data model:
/// `oagw_plugin` UNIQUE `(tenant_id, name)`), and two tenants may still use the
/// same name for their own definitions.
#[tokio::test]
async fn a_duplicate_plugin_name_is_refused_within_one_tenant_only() {
    let gateway = gateway();
    let created = post_json(&gateway, "/oagw/v1/plugins", tenant_a(), plugin_json("dup")).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the first definition is stored"
    );

    let mut response =
        post_json(&gateway, "/oagw/v1/plugins", tenant_a(), plugin_json("dup")).await;
    assert_eq!(
        status_of(&response).as_u16(),
        409,
        "the same name in the same tenant is a conflict"
    );
    assert_problem(&response, ErrorKind::Conflict.gts_fragment(), 409);
    let body = body_json(&mut response).await;
    assert_eq!(body["error_code"], json!("CONFLICT"));
    assert_eq!(
        problem_field(&body),
        "name",
        "the offending member is named"
    );

    // The other tenant is not affected by the first tenant's namespace.
    let created = post_json(&gateway, "/oagw/v1/plugins", tenant_b(), plugin_json("dup")).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the name is free in another tenant"
    );
}

#[tokio::test]
async fn a_plugin_referenced_by_a_chain_cannot_be_deleted_through_the_api() {
    let gateway = gateway();
    let mut created = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        plugin_json("order-guard"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let id = Uuid::parse_str(body_json(&mut created).await["id"].as_str().expect("id"))
        .expect("uuid id");

    // The reference is written through the store: the management API refuses a
    // custom-plugin binding on a chain, but a stored reference still blocks the
    // deletion (`ADR 0001` "Plugin Deletion Behavior").
    let upstream = crate::domain::model::Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant_a(),
        alias: "in-use.local".to_owned(),
        plugins: crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![crate::domain::model::PluginBindingDto::Ref(id.to_string())],
        },
        ..crate::domain::model::Upstream::default()
    };
    gateway
        .store
        .insert_upstream(upstream)
        .expect("the referencing upstream is stored");

    let mut response = delete(&gateway, &format!("/oagw/v1/plugins/{id}"), tenant_a()).await;
    assert_eq!(
        status_of(&response).as_u16(),
        409,
        "a referenced plugin stays"
    );
    assert_problem(&response, ErrorKind::PluginInUse.gts_fragment(), 409);
    let body = body_json(&mut response).await;
    assert_eq!(body["error_code"], json!("PLUGIN_IN_USE"));
    assert!(
        body["context"]["referenced_by"]["upstreams"]
            .as_array()
            .is_some_and(|references| references.len() == 1),
        "the problem names what still references the plugin, got: {body}"
    );
    assert!(gateway.control.get_plugin(tenant_a(), id).is_some());
}

#[tokio::test]
async fn a_malformed_plugin_definition_is_refused() {
    let gateway = gateway();

    let mut response = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        json!({ "plugin_type": "renderer", "name": "nope", "source_code": "x" }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "plugin_type");

    let mut response = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        json!({ "plugin_type": "transform", "name": "nope", "source_code": "  " }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "source_code");

    let mut response = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        json!({ "plugin_type": "transform", "name": "  ", "source_code": "x" }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_eq!(problem_field(&body_json(&mut response).await), "name");

    // Plugins are tenant-scoped: another tenant never sees one.
    let mut created = post_json(
        &gateway,
        "/oagw/v1/plugins",
        tenant_a(),
        plugin_json("scoped"),
    )
    .await;
    let id = Uuid::parse_str(body_json(&mut created).await["id"].as_str().expect("id"))
        .expect("uuid id");
    let foreign = get_json(&gateway, &format!("/oagw/v1/plugins/{id}"), tenant_b()).await;
    assert_eq!(status_of(&foreign).as_u16(), 404);
}

// ---------------------------------------------------------------------------
// Resource identifiers in path parameters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gts_resource_identifiers_resolve_like_bare_uuids() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9400, "identified.local").await;
    let id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");
    let gts_id = format!("{}{id}", crate::domain::model::UPSTREAM_GTS_BASE);

    let mut by_uuid = get_json(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_a()).await;
    assert_eq!(status_of(&by_uuid).as_u16(), 200);
    let mut by_gts = get_json(
        &gateway,
        &format!("/oagw/v1/upstreams/{gts_id}"),
        tenant_a(),
    )
    .await;
    assert_eq!(status_of(&by_gts).as_u16(), 200, "the GTS form resolves");
    assert_eq!(body_json(&mut by_uuid).await, body_json(&mut by_gts).await);

    // The same identifier deletes the same resource.
    let deleted = delete(
        &gateway,
        &format!("/oagw/v1/upstreams/{gts_id}"),
        tenant_a(),
    )
    .await;
    assert_eq!(status_of(&deleted).as_u16(), 204, "the GTS form deletes");
    let gone = get_json(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant_a()).await;
    assert_eq!(status_of(&gone).as_u16(), 404);

    // Routes resolve through the documented form too.
    let created = create_upstream(&gateway, "127.0.0.1", 9401, "routed.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");
    let mut route = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant_a(),
        route_json(upstream, "/v1", &["GET"]),
    )
    .await;
    let route_id =
        Uuid::parse_str(body_json(&mut route).await["id"].as_str().expect("id")).expect("uuid id");
    let mut fetched = get_json(
        &gateway,
        &format!(
            "/oagw/v1/routes/{}{route_id}",
            crate::domain::model::ROUTE_GTS_BASE
        ),
        tenant_a(),
    )
    .await;
    assert_eq!(status_of(&fetched).as_u16(), 200, "route GTS id resolves");
    assert_eq!(body_json(&mut fetched).await["id"], json!(route_id));

    // And a malformed identifier is a validation problem, not a 404.
    let mut response = get_json(&gateway, "/oagw/v1/upstreams/not-an-id", tenant_a()).await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    assert_eq!(problem_field(&body_json(&mut response).await), "id");
}

// ---------------------------------------------------------------------------
// Alias immutability on PUT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_cannot_change_the_alias_of_an_upstream() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9402, "stable.local").await;
    let id = created["id"].as_str().expect("id").to_owned();

    // An explicit alias that differs is refused, although the request is well
    // formed: the data plane is addressed by the alias, so `PUT` replaces the
    // definition and never its identity.
    let mut replacement = created.clone();
    replacement["alias"] = json!("renamed.local");
    let mut response = put_json(
        &gateway,
        &format!("/oagw/v1/upstreams/{id}"),
        tenant_a(),
        replacement,
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "the alias may not change"
    );
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "alias");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("immutable"),
        "the problem says the alias is immutable, got: {body}"
    );

    // A different tenant spelling of the same change is refused identically.
    let mut response = put_json(
        &gateway,
        &format!("/oagw/v1/upstreams/{id}"),
        tenant_a(),
        json!({
            "alias": "renamed.local",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9402 }] }
        }),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_eq!(
        problem_field(&body_json(&mut response).await),
        "alias",
        "the problem names the offending member"
    );

    // The stored alias is still served, and the definition is untouched.
    assert!(
        gateway
            .store
            .upstream_by_alias(tenant_a(), &normalize("stable.local"))
            .is_some(),
        "the alias still resolves to the same upstream"
    );
    assert!(
        gateway
            .store
            .upstream_by_alias(tenant_a(), &normalize("renamed.local"))
            .is_none(),
        "no upstream was created under the requested alias"
    );

    // Replacing the definition while keeping the alias is the supported move:
    // the identity survives and the definition is what changed.
    let mut replacement = created.clone();
    replacement["server"]["endpoints"][0]["port"] = json!(9403);
    let mut replaced = put_json(
        &gateway,
        &format!("/oagw/v1/upstreams/{id}"),
        tenant_a(),
        replacement,
    )
    .await;
    assert_eq!(
        status_of(&replaced).as_u16(),
        200,
        "the same alias replaces"
    );
    let body = body_json(&mut replaced).await;
    assert_eq!(body["alias"], json!("stable.local"));
    assert_eq!(body["server"]["endpoints"][0]["port"], json!(9403));
    assert_eq!(
        gateway
            .store
            .upstream_by_alias(tenant_a(), &normalize("stable.local"))
            .map(|upstream| upstream.server.endpoints[0].port),
        Some(Some(9403)),
        "the definition was replaced under the same alias"
    );
}

// ---------------------------------------------------------------------------
// Endpoint pool uniformity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_mixed_endpoint_pool_is_refused() {
    let gateway = gateway();
    let mixed = json!({
        "alias": "mixed.local",
        "server": {
            "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": 9404 },
                { "scheme": "https", "host": "127.0.0.1", "port": 9404 }
            ]
        }
    });
    let mut response = post_json(&gateway, "/oagw/v1/upstreams", tenant_a(), mixed).await;
    assert_eq!(status_of(&response).as_u16(), 400, "mixed schemes refused");
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "server.endpoints");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("scheme"),
        "the problem names the invariant, got: {body}"
    );

    let mixed_port = json!({
        "alias": "mixed-port.local",
        "server": {
            "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": 9405 },
                { "scheme": "http", "host": "127.0.0.1", "port": 9406 }
            ]
        }
    });
    let mut response = post_json(&gateway, "/oagw/v1/upstreams", tenant_a(), mixed_port).await;
    assert_eq!(status_of(&response).as_u16(), 400, "mixed ports refused");
    assert!(
        body_json(&mut response).await["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("port"),
        "the problem names the port invariant"
    );

    // A default port is the effective one: an explicit 443 on an https endpoint
    // and an omitted port are the same pool.
    let homogeneous = json!({
        "alias": "vendor.com",
        "server": {
            "endpoints": [
                { "scheme": "https", "host": "us.vendor.com", "port": 443 },
                { "scheme": "https", "host": "eu.vendor.com" }
            ]
        }
    });
    let mut response = post_json(&gateway, "/oagw/v1/upstreams", tenant_a(), homogeneous).await;
    assert_eq!(
        status_of(&response).as_u16(),
        201,
        "a uniform pool is accepted"
    );
    assert_eq!(body_json(&mut response).await["alias"], json!("vendor.com"));

    // Two explicit, equal, non-default ports are uniform too, and the derived
    // alias carries the port.
    let mut response = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant_a(),
        json!({
            "alias": "vendor.com:9443",
            "server": {
                "endpoints": [
                    { "scheme": "http", "host": "us.vendor.com", "port": 9443 },
                    { "scheme": "http", "host": "eu.vendor.com", "port": 9443 }
                ]
            }
        }),
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        201,
        "equal explicit ports are uniform"
    );
    assert_eq!(
        body_json(&mut response).await["alias"],
        json!("vendor.com:9443")
    );
}

// ---------------------------------------------------------------------------
// Rate-limit strategy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unimplemented_rate_limit_strategy_is_refused() {
    let gateway = gateway();
    for strategy in ["queue", "degrade"] {
        let mut limit = upstream_json("127.0.0.1", 9407, format!("{strategy}.local").as_str());
        limit["rate_limit"] = json!({
            "algorithm": "token_bucket",
            "sustained": { "rate": 10, "window": "minute" },
            "scope": "tenant",
            "strategy": strategy,
            "cost": 1,
            "response_headers": true
        });
        let mut response = post_json(&gateway, "/oagw/v1/upstreams", tenant_a(), limit).await;
        assert_eq!(
            status_of(&response).as_u16(),
            400,
            "'{strategy}' has no implementation and must not be accepted"
        );
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(problem_field(&body), "rate_limit.strategy");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains(strategy),
            "the problem names the unsupported strategy, got: {body}"
        );
    }

    // `reject` keeps working.
    let mut acceptable = upstream_json("127.0.0.1", 9408, "rejecting.local");
    acceptable["rate_limit"] = json!({
        "algorithm": "token_bucket",
        "sustained": { "rate": 10, "window": "minute" },
        "scope": "tenant",
        "strategy": "reject",
        "cost": 1,
        "response_headers": true
    });
    let response = post_json(&gateway, "/oagw/v1/upstreams", tenant_a(), acceptable).await;
    assert_eq!(status_of(&response).as_u16(), 201, "reject stays supported");
}

// ---------------------------------------------------------------------------
// Uniqueness under concurrency
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_upstream_creations_admit_one_winner() {
    let gateway = gateway();
    let contenders = 8;
    let barrier = Arc::new(std::sync::Barrier::new(contenders));
    // The handles are collected on purpose: joining lazily would run the
    // contenders one at a time, and the barrier they wait on needs all eight.
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..contenders)
        .map(|_| {
            let store = Arc::clone(&gateway.store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let upstream = crate::domain::model::Upstream {
                    id: Uuid::new_v4(),
                    tenant_id: tenant_a(),
                    alias: "race.local".to_owned(),
                    ..crate::domain::model::Upstream::default()
                };
                store.insert_upstream(upstream).is_ok()
            })
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().expect("contender joins"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(winners, 1, "exactly one creation may claim the alias");
    assert_eq!(
        gateway.store.upstreams_of(tenant_a()).len(),
        1,
        "the losers left nothing behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_routes_admit_one_winner() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9409, "racy.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");
    let contenders = 8;
    let barrier = Arc::new(std::sync::Barrier::new(contenders));
    // The handles are collected on purpose: joining lazily would run the
    // contenders one at a time, and the barrier they wait on needs all eight.
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..contenders)
        .map(|_| {
            let store = Arc::clone(&gateway.store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let route = Route {
                    id: Uuid::new_v4(),
                    tenant_id: tenant_a(),
                    upstream_id: upstream,
                    match_rule: RouteMatch {
                        http: Some(crate::domain::model::HttpMatch {
                            methods: vec![crate::domain::model::HttpMethod::Get],
                            path: "/v1".to_owned(),
                            query_allowlist: Vec::new(),
                            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                        }),
                        ..RouteMatch::default()
                    },
                    ..Route::default()
                };
                store.insert_route(route).is_ok()
            })
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().expect("contender joins"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(winners, 1, "one route may claim a (path, method) pair");
    assert_eq!(
        gateway.store.routes_for_upstream(upstream).len(),
        1,
        "the losers left nothing behind"
    );
}

// ---------------------------------------------------------------------------
// List query parameters
// ---------------------------------------------------------------------------

/// The aliases of a listed upstream collection, in the order it was returned.
async fn listed_aliases(gateway: &Gateway, query: &str) -> Vec<String> {
    let mut response = get_json(gateway, &format!("/oagw/v1/upstreams{query}"), tenant_a()).await;
    assert_eq!(status_of(&response).as_u16(), 200, "{query}");
    let body = body_json(&mut response).await;
    body.as_array()
        .unwrap_or_else(|| panic!("a list is a JSON array, got {body}"))
        .iter()
        .map(|entry| entry["alias"].as_str().expect("alias").to_owned())
        .collect()
}

/// The matched paths of a listed route collection, in the order it returned.
async fn listed_paths(gateway: &Gateway, query: &str) -> Vec<String> {
    let mut response = get_json(gateway, &format!("/oagw/v1/routes{query}"), tenant_a()).await;
    assert_eq!(status_of(&response).as_u16(), 200, "{query}");
    let body = body_json(&mut response).await;
    body.as_array()
        .unwrap_or_else(|| panic!("a list is a JSON array, got {body}"))
        .iter()
        .map(|entry| {
            entry["match"]["http"]["path"]
                .as_str()
                .expect("path")
                .to_owned()
        })
        .collect()
}

/// `$top`/`$skip` page, `$orderby` sorts, `$select` projects, and `$filter`
/// narrows the upstream collection (`DESIGN.md` §"List Query Parameters"); the
/// response stays a JSON array either way.
#[tokio::test]
async fn the_upstream_list_pages_sorts_projects_and_filters() {
    let gateway = gateway();
    for (alias, port) in [
        ("alpha.local", 9501),
        ("beta.local", 9502),
        ("gamma.local", 9503),
    ] {
        let created = create_upstream(&gateway, "127.0.0.1", port, alias).await;
        let _ = created;
    }

    // An unqualified list is the whole collection, capped at the default page.
    assert_eq!(
        listed_aliases(&gateway, "").await,
        vec!["alpha.local", "beta.local", "gamma.local"],
        "the default order is the storage order"
    );

    assert_eq!(
        listed_aliases(&gateway, "?$top=2").await.len(),
        2,
        "$top cuts the page"
    );
    assert_eq!(
        listed_aliases(&gateway, "?$top=2&$skip=1").await,
        vec!["beta.local", "gamma.local"],
        "$skip is applied before the page is cut"
    );
    assert_eq!(
        listed_aliases(&gateway, "?$orderby=alias%20desc").await,
        vec!["gamma.local", "beta.local", "alpha.local"],
        "$orderby sorts, and `desc` reverses"
    );

    let mut response = get_json(&gateway, "/oagw/v1/upstreams?$select=alias", tenant_a()).await;
    let body = body_json(&mut response).await;
    assert_eq!(
        body,
        json!([{ "alias": "alpha.local" }, { "alias": "beta.local" }, { "alias": "gamma.local" }]),
        "$select projects every row down to the named members"
    );

    let filter = "/oagw/v1/upstreams?$filter=alias%20eq%20%27beta.local%27";
    let mut response = get_json(&gateway, filter, tenant_a()).await;
    let body = body_json(&mut response).await;
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(1),
        "$filter keeps only the matching row"
    );
    assert_eq!(body[0]["alias"], json!("beta.local"));
}

/// A list option the endpoint cannot honour is a `400` problem naming the
/// offending parameter, never a silently different page.
#[tokio::test]
async fn a_malformed_list_option_is_a_400_problem() {
    let gateway = gateway();
    let _ = create_upstream(&gateway, "127.0.0.1", 9511, "solo.local").await;

    let cases = [
        ("?$top=abc", "$top"),
        ("?$top=101", "$top"),
        ("?$top=-1", "$top"),
        ("?$skip=soon", "$skip"),
        ("?$skip=-3", "$skip"),
        ("?$orderby=server", "$orderby"),
        ("?$orderby=alias%20sideways", "$orderby"),
        ("?$select=secret", "$select"),
        ("?$select=alias,secret", "$select"),
        ("?$select=alias,secret,tags", "$select"),
        ("?$filter=alias", "$filter"),
        ("?$filter=alias%20ne%20%27x%27", "$filter"),
        ("?$filter=server%20eq%20%27x%27", "$filter"),
    ];
    for (query, parameter) in cases {
        let uri = format!("/oagw/v1/upstreams{query}");
        let mut response = send(&gateway.router, request(Method::GET, &uri, tenant_a())).await;
        assert_eq!(status_of(&response).as_u16(), 400, "{query}");
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(
            problem_field(&body),
            parameter,
            "{query} names its parameter"
        );
    }
}

/// The route list speaks the same query language, and its own fields are the
/// ones it can be sorted and filtered by.
#[tokio::test]
async fn the_route_list_honours_the_documented_options() {
    let gateway = gateway();
    let created = create_upstream(&gateway, "127.0.0.1", 9512, "routes.local").await;
    let upstream = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid id");

    for (path, priority) in [("/low", 1), ("/high", 8), ("/mid", 4)] {
        let mut route = route_json(upstream, path, &["GET"]);
        route["priority"] = json!(priority);
        let mut response = post_json(&gateway, "/oagw/v1/routes", tenant_a(), route).await;
        assert_eq!(status_of(&response).as_u16(), 201, "{path}");
        let _ = body_json(&mut response).await;
    }

    let paths = |query: &'static str| listed_paths(&gateway, query);
    assert_eq!(
        paths("?$orderby=priority%20desc").await,
        vec!["/high", "/mid", "/low"],
        "the numeric priority sorts"
    );
    assert_eq!(
        paths("?$filter=priority%20eq%208").await,
        vec!["/high"],
        "the numeric priority filters"
    );
    // Without an `$orderby` the window is the storage order, which is the
    // creation order here.
    assert_eq!(
        paths("?$top=2&$skip=1").await,
        vec!["/high", "/mid"],
        "paging works on the route collection"
    );

    let mut response = get_json(&gateway, "/oagw/v1/routes?$select=match", tenant_a()).await;
    let body = body_json(&mut response).await;
    for entry in body.as_array().expect("a list") {
        assert_eq!(
            entry.as_object().map(serde_json::Map::len),
            Some(1),
            "a projected route row carries only `match`, got {entry}"
        );
    }
}

/// The plugin catalogue pages like the other collections, built-ins included.
#[tokio::test]
async fn the_plugin_catalogue_pages_and_sorts() {
    let gateway = gateway();
    let mut response = get_json(&gateway, "/oagw/v1/plugins", tenant_a()).await;
    let all = body_json(&mut response).await;
    let total = all.as_array().expect("catalogue").len();
    assert!(total > 1, "the built-ins are catalogued");

    let mut response = get_json(&gateway, "/oagw/v1/plugins?$top=1", tenant_a()).await;
    let page = body_json(&mut response).await;
    assert_eq!(
        page.as_array().map(Vec::len),
        Some(1),
        "$top cuts the catalogue"
    );
    assert_eq!(page[0], all[0], "the page is the head of the catalogue");

    let mut response = get_json(&gateway, "/oagw/v1/plugins?$top=2&$skip=1", tenant_a()).await;
    let page = body_json(&mut response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(2));
    assert_eq!(page[0], all[1], "$skip moves the page window");

    // A name sort is stable and the projected rows carry nothing else.
    let mut response = get_json(
        &gateway,
        "/oagw/v1/plugins?$orderby=name&$select=name",
        tenant_a(),
    )
    .await;
    let body = body_json(&mut response).await;
    let names: Vec<&str> = body
        .as_array()
        .expect("catalogue")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "the catalogue is sorted by name");
    for entry in body.as_array().expect("catalogue") {
        assert_eq!(
            entry.as_object().map(serde_json::Map::len),
            Some(1),
            "a projected row carries only `name`, got {entry}"
        );
    }
}

/// The list operations advertise the query options they honour, so a generated
/// client learns them from the document instead of from `DESIGN.md`.
#[tokio::test]
async fn the_list_operations_advertise_the_documented_query_options() {
    let gateway = gateway();
    let operations = [
        "oagw.list_upstreams",
        "oagw.list_routes",
        "oagw.list_plugins",
    ];
    for operation_id in operations {
        let spec = gateway
            .openapi
            .operation_specs
            .iter()
            .map(|entry| entry.value().clone())
            .find(|spec| spec.operation_id.as_deref() == Some(operation_id))
            .unwrap_or_else(|| panic!("{operation_id} is registered"));
        let names: Vec<&str> = spec
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect();
        for option in ["$top", "$skip", "$orderby", "$select", "$filter"] {
            assert!(
                names.contains(&option),
                "{operation_id} advertises {option}, got {names:?}"
            );
        }
    }
}

/// Every relayable verb on the proxy paths is declared anonymous, not just the
/// `GET` that carries the full operation.
///
/// The gateway derives its authentication policy from one operation spec per
/// `(method, path)`: a verb with no spec falls through to
/// `require_auth_by_default` and is refused `401` before the relay ever runs,
/// even though the axum route itself answers every method.
#[tokio::test]
async fn every_relayed_verb_is_declared_anonymous_on_the_proxy_paths() {
    let gateway = gateway();
    let proxy_paths = ["/oagw/v1/proxy/{alias}/{*path}", "/oagw/v1/proxy/{alias}"];
    for path in proxy_paths {
        let verbs: Vec<(String, bool)> = gateway
            .openapi
            .operation_specs
            .iter()
            .filter(|entry| entry.value().path == path)
            .map(|entry| {
                let spec = entry.value();
                (spec.method.as_str().to_owned(), spec.authenticated)
            })
            .collect();
        for verb in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
            let declared = verbs.iter().find(|(name, _)| name == verb);
            assert_eq!(
                declared.map(|(_, authenticated)| *authenticated),
                Some(false),
                "{verb} {path} is registered and anonymous, got {verbs:?}"
            );
        }
    }
}
