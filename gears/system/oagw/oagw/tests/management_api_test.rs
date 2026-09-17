// Created: 2026-09-03 by Constructor Tech
//! Control-plane integration tests for the OAGW management API.
//!
//! Drives the real axum router with `Router::oneshot` and asserts on the
//! RFC 9457 problem envelope for every failure mode: status, `type`, `title`,
//! `detail`, `X-OAGW-Error-Source: gateway` and `context.fields`.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::large_types_passed_by_value,
    clippy::redundant_clone
)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use common::{PROTOCOL_GRPC, PROTOCOL_HTTP, TENANT_A, TENANT_B};

// ── Fixtures ────────────────────────────────────────────────────────────

/// `POST /oagw/v1/upstreams` for `endpoints` with the HTTP protocol.
async fn create_upstream(app: Router, endpoints: Value) -> Response<Body> {
    common::post(
        "/oagw/v1/upstreams",
        json!({ "server": { "endpoints": endpoints }, "protocol": PROTOCOL_HTTP }),
    )
    .send(app)
    .await
}

/// Create a single-endpoint upstream and return the derived alias.
async fn derived_alias(endpoints: Value) -> String {
    let body = common::expect_json(create_upstream(common::app(false), endpoints).await, 201).await;
    body["alias"].as_str().expect("alias is a string").to_owned()
}

/// Create one upstream owned by `tenant` and return the response body.
async fn upstream_for(app: &Router, tenant: Uuid, host: &str) -> Value {
    let response = common::post(
        "/oagw/v1/upstreams",
        json!({
            "server": { "endpoints": [ { "scheme": "https", "host": host, "port": 443 } ] },
            "protocol": PROTOCOL_HTTP
        }),
    )
    .tenant(tenant)
    .send(app.clone())
    .await;
    common::expect_json(response, 201).await
}

/// An HTTPS upstream for `TENANT_A` that serves `/v1` with `methods`.
async fn http_upstream_with_route(
    app: &Router,
    methods: &[&str],
    path: &str,
    suffix_mode: &str,
) -> (Value, Uuid) {
    let upstream = upstream_for(app, TENANT_A, "api.openai.com").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": {
            "methods": methods,
            "path": path,
            "query_allowlist": [],
            "path_suffix_mode": suffix_mode
        }}
    });
    let response = common::post("/oagw/v1/routes", body).send(app.clone()).await;
    let route = common::expect_json(response, 201).await;
    (upstream, route["id"].as_str().expect("route id").parse().expect("uuid"))
}

/// Create a gRPC upstream for `TENANT_A`.
async fn grpc_upstream(app: &Router) -> Value {
    let response = common::post(
        "/oagw/v1/upstreams",
        json!({
            "server": { "endpoints": [ { "scheme": "grpc", "host": "grpc.vendor.com", "port": 443 } ] },
            "protocol": PROTOCOL_GRPC
        }),
    )
    .send(app.clone())
    .await;
    common::expect_json(response, 201).await
}

/// Replace `upstream_id` with `patch` merged into a full PUT body.
async fn replace_upstream(app: &Router, upstream_id: Uuid, patch: Value) -> Response<Body> {
    let mut full = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    if let (Some(target), Some(patch)) = (full.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            target.insert(key.clone(), value.clone());
        }
    }
    common::put(format!("/oagw/v1/upstreams/{upstream_id}"), full)
        .send(app.clone())
        .await
}

// ── Alias derivation table ──────────────────────────────────────────────

#[tokio::test]
async fn single_host_with_standard_port_omits_the_port_from_the_alias() {
    assert_eq!(
        derived_alias(json!([{ "scheme": "https", "host": "api.openai.com", "port": 443 }])).await,
        "api.openai.com"
    );
}

#[tokio::test]
async fn single_host_with_non_standard_port_keeps_the_port_in_the_alias() {
    assert_eq!(
        derived_alias(json!([{ "scheme": "https", "host": "api.openai.com", "port": 8443 }])).await,
        "api.openai.com:8443"
    );
}

#[tokio::test]
async fn pool_derives_the_registrable_common_suffix() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.vendor.com", "port": 443 },
        { "scheme": "https", "host": "eu.vendor.com", "port": 443 }
    ]);
    assert_eq!(derived_alias(endpoints).await, "vendor.com");
}

#[tokio::test]
async fn pool_derives_the_common_suffix_with_the_shared_port() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.vendor.com", "port": 8443 },
        { "scheme": "https", "host": "eu.vendor.com", "port": 8443 }
    ]);
    assert_eq!(derived_alias(endpoints).await, "vendor.com:8443");
}

#[tokio::test]
async fn pool_sharing_a_bare_public_suffix_requires_an_explicit_alias() {
    let endpoints = json!([
        { "scheme": "https", "host": "foo.co.uk", "port": 443 },
        { "scheme": "https", "host": "bar.co.uk", "port": 443 }
    ]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "an explicit alias is required for IP-based or non-derivable endpoints",
        &["alias"],
    )
    .await;
}

#[tokio::test]
async fn pool_sharing_a_bare_public_suffix_accepts_an_explicit_alias() {
    let app = common::app(false);
    let body = json!({
        "alias": "uk-pool",
        "server": { "endpoints": [
            { "scheme": "https", "host": "foo.co.uk", "port": 443 },
            { "scheme": "https", "host": "bar.co.uk", "port": 443 }
        ]},
        "protocol": PROTOCOL_HTTP
    });
    let created = common::expect_json(common::post("/oagw/v1/upstreams", body).send(app).await, 201).await;
    assert_eq!(created["alias"], "uk-pool");
}

#[tokio::test]
async fn heterogeneous_pool_requires_an_explicit_alias() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.foo.com", "port": 443 },
        { "scheme": "https", "host": "eu.bar.com", "port": 443 }
    ]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "an explicit alias is required",
        &["alias"],
    )
    .await;
}

#[tokio::test]
async fn ip_endpoints_require_an_explicit_alias() {
    let endpoints = json!([{ "scheme": "https", "host": "10.0.1.1", "port": 443 }]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "an explicit alias is required",
        &["alias"],
    )
    .await;
}

#[tokio::test]
async fn pool_with_mixed_ports_is_rejected() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.vendor.com", "port": 443 },
        { "scheme": "https", "host": "eu.vendor.com", "port": 8443 }
    ]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "all endpoints in a pool must use the same scheme and port",
        &["server.endpoints[].scheme"],
    )
    .await;
}

#[tokio::test]
async fn pool_with_mixed_schemes_is_rejected() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.vendor.com", "port": 443 },
        { "scheme": "wss", "host": "eu.vendor.com", "port": 443 }
    ]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "all endpoints in a pool must use the same scheme and port",
        &["server.endpoints[].scheme"],
    )
    .await;
}

#[tokio::test]
async fn explicit_alias_must_match_the_derived_alias() {
    let app = common::app(false);
    let body = json!({
        "alias": "my-openai",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "does not match the auto-derived alias 'api.openai.com'",
        &["alias"],
    )
    .await;
}

#[tokio::test]
async fn explicit_alias_equal_to_the_derivation_is_idempotent() {
    let app = common::app(false);
    let body = json!({
        "alias": "api.openai.com",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let created = common::expect_json(common::post("/oagw/v1/upstreams", body).send(app).await, 201).await;
    assert_eq!(created["alias"], "api.openai.com");
    assert_eq!(created["server"]["endpoints"][0]["host"], "api.openai.com");
    assert_eq!(created["protocol"], PROTOCOL_HTTP);
    assert!(created["id"].is_string());
    assert_eq!(created["tenant_id"], TENANT_A.to_string());
    assert!(created["created_at"].is_string());
    assert!(created["updated_at"].is_string());
}

#[tokio::test]
async fn explicit_alias_is_normalized_before_it_is_validated() {
    let app = common::app(false);
    let body = json!({
        "alias": "API.OpenAI.COM.",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let created = common::expect_json(common::post("/oagw/v1/upstreams", body).send(app).await, 201).await;
    assert_eq!(created["alias"], "api.openai.com");
}

#[tokio::test]
async fn explicit_alias_must_match_the_alias_pattern() {
    let app = common::app(false);
    let body = json!({
        "alias": "-not-allowed-",
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.1.1", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "does not match the required pattern",
        &["alias"],
    )
    .await;
}

#[tokio::test]
async fn trailing_dot_and_case_are_normalized_in_derived_aliases() {
    let endpoints = json!([{ "scheme": "https", "host": "API.OpenAI.COM.", "port": 443 }]);
    assert_eq!(derived_alias(endpoints).await, "api.openai.com");
}

// ── Endpoint schemes and host validation ────────────────────────────────

#[tokio::test]
async fn plaintext_scheme_requires_the_allow_http_flag() {
    let endpoints = json!([{ "scheme": "http", "host": "upstream.internal", "port": 9000 }]);
    let rejected = create_upstream(common::app(false), endpoints.clone()).await;
    common::expect_problem(
        rejected,
        400,
        "Validation Error",
        "oagw.config.allow_http_upstream",
        &["server.endpoints[].scheme"],
    )
    .await;

    let accepted = create_upstream(common::app(true), endpoints).await;
    let body = common::expect_json(accepted, 201).await;
    assert_eq!(body["alias"], "upstream.internal:9000");
}

#[tokio::test]
async fn plaintext_scheme_is_allowed_when_the_flag_is_set() {
    let app = common::app(true);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8080 } ] },
        "protocol": PROTOCOL_HTTP
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(response, 400, "Validation Error", "an explicit alias is required", &["alias"])
        .await;
}

#[tokio::test]
async fn unsupported_scheme_is_rejected() {
    let endpoints = json!([{ "scheme": "ftp", "host": "api.openai.com", "port": 443 }]);
    let response = create_upstream(common::app(true), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "unsupported endpoint scheme 'ftp'",
        &["server.endpoints[].scheme"],
    )
    .await;
}

#[tokio::test]
async fn every_documented_scheme_is_accepted() {
    for scheme in ["https", "wss", "wt", "grpc"] {
        let endpoints = json!([{ "scheme": scheme, "host": "api.openai.com", "port": 443 }]);
        assert_eq!(
            derived_alias(endpoints).await,
            "api.openai.com",
            "scheme {scheme} must be accepted"
        );
    }
}

#[tokio::test]
async fn empty_endpoint_pool_is_rejected() {
    let response = common::post(
        "/oagw/v1/upstreams",
        json!({ "server": { "endpoints": [] }, "protocol": PROTOCOL_HTTP }),
    )
    .send(common::app(false))
    .await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "server.endpoints must contain at least one endpoint",
        &["server.endpoints"],
    )
    .await;
}

#[tokio::test]
async fn missing_server_block_is_rejected() {
    let response = common::post("/oagw/v1/upstreams", json!({ "protocol": PROTOCOL_HTTP }))
        .send(common::app(false))
        .await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "server.endpoints must contain at least one endpoint",
        &["server.endpoints"],
    )
    .await;
}

#[tokio::test]
async fn zero_port_is_rejected() {
    let endpoints = json!([{ "scheme": "https", "host": "api.openai.com", "port": 0 }]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "endpoint port must be between 1 and 65535",
        &["server.endpoints[].port"],
    )
    .await;
}

#[tokio::test]
async fn invalid_hostname_is_rejected() {
    for host in ["-bad.example.com", "bad..com", "under_score.example.com"] {
        let endpoints = json!([{ "scheme": "https", "host": host, "port": 443 }]);
        let response = create_upstream(common::app(false), endpoints).await;
        common::expect_problem(
            response,
            400,
            "Validation Error",
            "is not a valid RFC 1123 hostname or IP address",
            &["server.endpoints[].host"],
        )
        .await;
    }
}

#[tokio::test]
async fn empty_hostname_is_rejected() {
    let endpoints = json!([{ "scheme": "https", "host": "", "port": 443 }]);
    let response = create_upstream(common::app(false), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "endpoint host must not be empty",
        &["server.endpoints[].host"],
    )
    .await;
}

#[tokio::test]
async fn pool_with_mixed_scheme_and_port_is_rejected() {
    let endpoints = json!([
        { "scheme": "https", "host": "us.vendor.com", "port": 443 },
        { "scheme": "http", "host": "eu.vendor.com", "port": 8443 }
    ]);
    let response = create_upstream(common::app(true), endpoints).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "all endpoints in a pool must use the same scheme and port",
        &["server.endpoints[].scheme"],
    )
    .await;
}

// ── Protocol validation ─────────────────────────────────────────────────

#[tokio::test]
async fn protocol_is_required() {
    let response = common::post(
        "/oagw/v1/upstreams",
        json!({ "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] } }),
    )
    .send(common::app(false))
    .await;
    common::expect_problem(response, 400, "Validation Error", "protocol is required", &["protocol"]).await;
}

#[tokio::test]
async fn unrecognized_protocol_is_rejected() {
    let response = common::post(
        "/oagw/v1/upstreams",
        json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
            "protocol": "http/1.1"
        }),
    )
    .send(common::app(false))
    .await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "protocol 'http/1.1' is not a recognized OAGW protocol identifier",
        &["protocol"],
    )
    .await;
}

#[tokio::test]
async fn grpc_protocol_is_accepted() {
    let upstream = grpc_upstream(&common::app(false)).await;
    assert_eq!(upstream["protocol"], PROTOCOL_GRPC);
    assert_eq!(upstream["alias"], "grpc.vendor.com");
}

// ── Upstream CRUD ───────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_crud_round_trip() {
    let app = common::app(false);
    let created = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");

    let fetched = common::Outgoing::new(Method::GET, format!("/oagw/v1/upstreams/{id}"))
        .send(app.clone())
        .await;
    let got = common::expect_json(fetched, 200).await;
    assert_eq!(got["alias"], "api.openai.com");

    let list = common::Outgoing::new(Method::GET, "/oagw/v1/upstreams")
        .send(app.clone())
        .await;
    let items = common::expect_json(list, 200).await;
    assert_eq!(items.as_array().expect("array").len(), 1);

    let tags = json!({
        "tags": ["ai", "production"],
        "enabled": false,
        "rate_limit": { "sustained": { "rate": 5, "window": "minute" }, "burst": { "capacity": 10 } }
    });
    let replaced = replace_upstream(&app, id, tags).await;
    let updated = common::expect_json(replaced, 200).await;
    assert_eq!(updated["tags"], json!(["ai", "production"]));
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["rate_limit"]["sustained"]["rate"], 5);
    assert_eq!(updated["id"], id.to_string());

    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/upstreams/{id}"))
            .send(app.clone())
            .await,
    )
    .await;

    let missing = common::Outgoing::new(Method::GET, format!("/oagw/v1/upstreams/{id}"))
        .send(app.clone())
        .await;
    common::expect_problem(missing, 404, "Route Not Found", "not found", &[]).await;
}

#[tokio::test]
async fn deleting_an_unknown_upstream_returns_404() {
    let app = common::app(false);
    let missing = Uuid::new_v4();
    let response = common::Outgoing::new(Method::DELETE, format!("/oagw/v1/upstreams/{missing}"))
        .send(app)
        .await;
    common::expect_problem(response, 404, "Route Not Found", "not found", &[]).await;
}

#[tokio::test]
async fn upstream_alias_is_immutable_on_replace() {
    let app = common::app(false);
    let created = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");

    let changed = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.other.com", "port": 443 } ] }
    });
    let response = replace_upstream(&app, id, changed).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "delete and re-create instead",
        &["alias"],
    )
    .await;

    let renamed = json!({
        "alias": "api.another.com",
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] }
    });
    let response = replace_upstream(&app, id, renamed).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "alias is immutable once set",
        &["alias"],
    )
    .await;

    let unchanged = common::expect_json(replace_upstream(&app, id, json!({})).await, 200).await;
    assert_eq!(unchanged["alias"], "api.openai.com");
}

#[tokio::test]
async fn upstream_replace_requires_the_protocol_field() {
    let app = common::app(false);
    let created = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");
    let response = common::put(
        format!("/oagw/v1/upstreams/{id}"),
        json!({ "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] } }),
    )
    .send(app)
    .await;
    common::expect_problem(response, 400, "Validation Error", "protocol is required", &["protocol"]).await;
}

#[tokio::test]
async fn duplicate_alias_conflicts_within_a_tenant() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP
    });
    common::expect_json(common::post("/oagw/v1/upstreams", body.clone()).send(app.clone()).await, 201).await;
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(response, 409, "Already Exists", "already exists for this tenant", &[]).await;
}

// ── Tenant isolation ────────────────────────────────────────────────────

#[tokio::test]
async fn resources_are_invisible_across_tenants() {
    let app = common::app(false);
    let created = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");

    let foreign = common::Outgoing::new(Method::GET, format!("/oagw/v1/upstreams/{id}"))
        .tenant(TENANT_B)
        .send(app.clone())
        .await;
    common::expect_problem(foreign, 404, "Route Not Found", "not found", &[]).await;

    let forbidden = common::Outgoing::new(Method::DELETE, format!("/oagw/v1/upstreams/{id}"))
        .tenant(TENANT_B)
        .send(app.clone())
        .await;
    common::expect_problem(forbidden, 404, "Route Not Found", "not found", &[]).await;

    let list = common::Outgoing::new(Method::GET, "/oagw/v1/upstreams")
        .tenant(TENANT_B)
        .send(app.clone())
        .await;
    let items = common::expect_json(list, 200).await;
    assert!(items.as_array().expect("array").is_empty());

    let own = common::Outgoing::new(Method::GET, format!("/oagw/v1/upstreams/{id}"))
        .send(app.clone())
        .await;
    assert_eq!(own.status().as_u16(), 200);
}

#[tokio::test]
async fn the_same_alias_may_exist_in_two_tenants() {
    let app = common::app(false);
    let first = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let second = upstream_for(&app, TENANT_B, "api.openai.com").await;
    assert_eq!(first["alias"], second["alias"]);
    assert_ne!(first["tenant_id"], second["tenant_id"]);
}

#[tokio::test]
async fn a_route_may_not_reference_a_foreign_upstream() {
    let app = common::app(false);
    let foreign = upstream_for(&app, TENANT_B, "api.openai.com").await;
    let body = json!({
        "upstream_id": foreign["id"],
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "not found for this tenant",
        &["upstream_id"],
    )
    .await;
}

#[tokio::test]
async fn resources_require_a_security_context() {
    let app = common::app(false);
    let response = common::Outgoing::new(Method::GET, "/oagw/v1/upstreams")
        .unauthenticated()
        .send(app)
        .await;
    assert_eq!(response.status().as_u16(), 500, "missing extension surfaces as a 500");
}

// ── Route validation matrix ─────────────────────────────────────────────

#[tokio::test]
async fn route_crud_round_trip() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET", "POST"], "/v1", "append").await;
    let upstream_id: Uuid = upstream["id"].as_str().expect("id").parse().expect("uuid");

    let list = common::Outgoing::new(
        Method::GET,
        format!("/oagw/v1/routes?upstream_id={upstream_id}"),
    )
    .send(app.clone())
    .await;
    let items = common::expect_json(list, 200).await;
    let routes = items.as_array().expect("array");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0]["upstream_id"], upstream_id.to_string());
    assert_eq!(routes[0]["tenant_id"], TENANT_A.to_string());
    assert_eq!(routes[0]["match"]["http"]["path"], "/v1");
    assert_eq!(routes[0]["match"]["http"]["path_suffix_mode"], "append");

    let all = common::Outgoing::new(Method::GET, "/oagw/v1/routes")
        .send(app.clone())
        .await;
    assert_eq!(common::expect_json(all, 200).await.as_array().expect("array").len(), 1);

    let updated = common::expect_json(
        common::put(
            format!("/oagw/v1/routes/{}", routes[0]["id"].as_str().expect("id")),
            json!({ "match": { "http": { "methods": ["PUT"], "path": "/v2" } } }),
        )
        .send(app.clone())
        .await,
        200,
    )
    .await;
    assert_eq!(updated["match"]["http"]["path"], "/v2");
    assert_eq!(updated["match"]["http"]["methods"], json!(["PUT"]));

    let route_id: Uuid = routes[0]["id"].as_str().expect("id").parse().expect("uuid");
    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/routes/{route_id}"))
            .send(app.clone())
            .await,
    )
    .await;

    let missing = common::Outgoing::new(Method::GET, format!("/oagw/v1/routes/{route_id}"))
        .send(app.clone())
        .await;
    common::expect_problem(missing, 404, "Route Not Found", "not found", &[]).await;
}

#[tokio::test]
async fn route_requires_an_upstream_id() {
    let body = json!({ "match": { "http": { "methods": ["GET"], "path": "/v1" } } });
    let response = common::post("/oagw/v1/routes", body).send(common::app(false)).await;
    common::expect_problem(response, 400, "Validation Error", "upstream_id is required", &["upstream_id"])
        .await;
}

#[tokio::test]
async fn route_requires_a_match_rule() {
    let app = common::app(false);
    let upstream = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let body = json!({ "upstream_id": upstream["id"] });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(response, 400, "Validation Error", "match is required", &["match"]).await;
}

#[tokio::test]
async fn route_to_an_unknown_upstream_is_rejected() {
    let body = json!({
        "upstream_id": Uuid::new_v4(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(common::app(false)).await;
    common::expect_problem(response, 400, "Validation Error", "not found for this tenant", &["upstream_id"])
        .await;
}

#[tokio::test]
async fn http_match_requires_an_http_upstream() {
    let app = common::app(false);
    let upstream = grpc_upstream(&app).await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "http match rule requires an upstream with the http protocol",
        &["match.http"],
    )
    .await;
}

#[tokio::test]
async fn grpc_match_requires_a_grpc_upstream() {
    let app = common::app(false);
    let upstream = upstream_for(&app, TENANT_A, "api.openai.com").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "grpc": { "service": "vendor.user.v1.UserService", "method": "GetUser" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "grpc match rule requires an upstream with the grpc protocol",
        &["match.grpc"],
    )
    .await;
}

#[tokio::test]
async fn grpc_match_against_a_grpc_upstream_is_accepted() {
    let app = common::app(false);
    let upstream = grpc_upstream(&app).await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "grpc": { "service": "vendor.user.v1.UserService", "method": "GetUser" } }
    });
    let created = common::expect_json(common::post("/oagw/v1/routes", body).send(app).await, 201).await;
    assert_eq!(created["match"]["grpc"]["service"], "vendor.user.v1.UserService");
}

#[tokio::test]
async fn route_with_no_methods_is_rejected() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": [], "path": "/v2" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "match.http.methods must contain at least one method",
        &["match.http.methods"],
    )
    .await;
}

#[tokio::test]
async fn route_with_an_invalid_method_token_is_rejected() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": ["NOT A METHOD"], "path": "/v2" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "is not a valid HTTP method",
        &["match.http.methods"],
    )
    .await;
}

#[tokio::test]
async fn route_path_must_be_absolute() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    for path in ["", "v1/no-slash"] {
        let body = json!({
            "upstream_id": upstream["id"],
            "match": { "http": { "methods": ["GET"], "path": path } }
        });
        let response = common::post("/oagw/v1/routes", body).send(app.clone()).await;
        common::expect_problem(
            response,
            400,
            "Validation Error",
            "must be an absolute path starting with '/'",
            &["match.http.path"],
        )
        .await;
    }
}

#[tokio::test]
async fn duplicate_route_match_rule_conflicts() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(response, 409, "Already Exists", "same match rule", &[]).await;
}

#[tokio::test]
async fn deleting_an_upstream_deletes_its_routes() {
    let app = common::app(false);
    let (upstream, route_id) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    let upstream_id: Uuid = upstream["id"].as_str().expect("id").parse().expect("uuid");

    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/upstreams/{upstream_id}"))
            .send(app.clone())
            .await,
    )
    .await;

    let response = common::Outgoing::new(Method::GET, format!("/oagw/v1/routes/{route_id}"))
        .send(app)
        .await;
    common::expect_problem(response, 404, "Route Not Found", "not found", &[]).await;
}

// ── CORS, rate-limit and plugin reference validation ────────────────────

#[tokio::test]
async fn cors_credentials_with_wildcard_origins_is_rejected() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true,
            "allowed_methods": ["GET"]
        }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "cors.allow_credentials is incompatible with allowed_origins ['*']",
        &["cors.allowed_origins"],
    )
    .await;
}

#[tokio::test]
async fn cors_origin_must_be_a_valid_uri() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "cors": { "enabled": true, "allowed_origins": ["not-a-uri"] }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "'not-a-uri' is not a valid URI",
        &["cors.allowed_origins"],
    )
    .await;
}

#[tokio::test]
async fn cors_method_must_be_a_valid_http_method() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["NOT A METHOD"] }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "is not a valid HTTP method",
        &["cors.allowed_methods"],
    )
    .await;
}

#[tokio::test]
async fn rate_limit_zero_sustained_rate_is_rejected() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "rate_limit": { "sustained": { "rate": 0 } }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "rate_limit.sustained.rate must be >= 1",
        &["rate_limit.sustained.rate"],
    )
    .await;
}

#[tokio::test]
async fn rate_limit_zero_burst_capacity_is_rejected() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "rate_limit": { "sustained": { "rate": 10 }, "burst": { "capacity": 0 } }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "rate_limit.burst.capacity must be >= 1",
        &["rate_limit.burst.capacity"],
    )
    .await;
}

#[tokio::test]
async fn rate_limit_zero_cost_is_rejected() {
    let app = common::app(false);
    let (upstream, _) = http_upstream_with_route(&app, &["GET"], "/v1", "append").await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": ["GET"], "path": "/v2" } },
        "rate_limit": { "sustained": { "rate": 10 }, "cost": 0 }
    });
    let response = common::post("/oagw/v1/routes", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "rate_limit.cost must be >= 1",
        &["rate_limit.cost"],
    )
    .await;
}

#[tokio::test]
async fn unknown_plugin_reference_is_rejected() {
    let app = common::app(false);
    let body = json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": PROTOCOL_HTTP,
        "plugins": { "items": ["not-a-plugin"] }
    });
    let response = common::post("/oagw/v1/upstreams", body).send(app).await;
    common::expect_problem(
        response,
        400,
        "Validation Error",
        "is neither a recognized GTS plugin identifier nor a UUID",
        &["plugins.items[].plugin_ref"],
    )
    .await;
}

// ── Plugins ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_crud_round_trip() {
    let app = common::app(false);
    let source = "def run(ctx):\n    return ctx\n";
    let body = json!({
        "plugin_type": "transform_plugin",
        "name": "redact-headers",
        "config_schema": { "type": "object" },
        "source_code": source
    });
    let created = common::expect_json(common::post("/oagw/v1/plugins", body).send(app.clone()).await, 201).await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");
    assert_eq!(created["plugin_type"], "transform_plugin");
    assert_eq!(created["name"], "redact-headers");
    assert_eq!(created["tenant_id"], TENANT_A.to_string());
    assert_eq!(created["source_code"], source);

    let fetched = common::Outgoing::new(Method::GET, format!("/oagw/v1/plugins/{id}"))
        .send(app.clone())
        .await;
    assert_eq!(common::expect_json(fetched, 200).await["id"], id.to_string());

    let listed = common::Outgoing::new(Method::GET, "/oagw/v1/plugins")
        .send(app.clone())
        .await;
    assert_eq!(common::expect_json(listed, 200).await.as_array().expect("array").len(), 1);

    let source_response = common::Outgoing::new(Method::GET, format!("/oagw/v1/plugins/{id}/source"))
        .send(app.clone())
        .await;
    assert_eq!(source_response.status().as_u16(), 200);
    assert_eq!(
        source_response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/x-python; charset=utf-8")
    );
    let (_, bytes) = common::drain(source_response).await;
    assert_eq!(bytes, source);

    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/plugins/{id}"))
            .send(app.clone())
            .await,
    )
    .await;

    let missing = common::Outgoing::new(Method::DELETE, format!("/oagw/v1/plugins/{id}"))
        .send(app)
        .await;
    common::expect_problem(missing, 404, "Route Not Found", "not found", &[]).await;
}

#[tokio::test]
async fn plugin_requires_a_type_and_a_name() {
    let app = common::app(false);
    let response = common::post("/oagw/v1/plugins", json!({ "name": "unnamed" }))
        .send(app.clone())
        .await;
    common::expect_problem(response, 400, "Validation Error", "plugin_type is required", &["plugin_type"])
        .await;

    let response = common::post("/oagw/v1/plugins", json!({ "plugin_type": "guard_plugin" }))
        .send(app)
        .await;
    common::expect_problem(response, 400, "Validation Error", "name is required", &["name"]).await;
}

#[tokio::test]
async fn bound_plugin_cannot_be_deleted() {
    let app = common::app(false);
    let plugin = common::expect_json(
        common::post(
            "/oagw/v1/plugins",
            json!({ "plugin_type": "guard_plugin", "name": "bound", "source_code": "def run(ctx):\n    return ctx\n" }),
        )
        .send(app.clone())
        .await,
        201,
    )
    .await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let upstream = common::expect_json(
        common::post(
            "/oagw/v1/upstreams",
            json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": PROTOCOL_HTTP,
                "plugins": { "items": [ plugin_id ] }
            }),
        )
        .send(app.clone())
        .await,
        201,
    )
    .await;
    assert_eq!(
        upstream["plugins"]["items"][0]["plugin_ref"],
        plugin_id.clone()
    );

    let response = common::Outgoing::new(Method::DELETE, format!("/oagw/v1/plugins/{plugin_id}"))
        .send(app.clone())
        .await;
    common::expect_problem(response, 409, "Plugin In Use", "still referenced", &[]).await;

    let upstream_id: Uuid = upstream["id"].as_str().expect("id").parse().expect("uuid");
    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/upstreams/{upstream_id}"))
            .send(app.clone())
            .await,
    )
    .await;

    common::expect_no_content(
        common::Outgoing::new(Method::DELETE, format!("/oagw/v1/plugins/{plugin_id}"))
            .send(app)
            .await,
    )
    .await;
}
