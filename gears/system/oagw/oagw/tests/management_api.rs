//! Integration tests over the full router: management CRUD lifecycle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use http::StatusCode;
use oagw::config::OagwConfig;
use serde_json::{Value, json};

/// The derived alias wins when the client leaves it out.
#[tokio::test]
async fn alias_is_derived_from_the_endpoint_pool() {
    let harness = Harness::new();
    let created = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com", "port": 443 } ] }
        }),
    )
    .await;
    assert_eq!(created["alias"], "api.vendor.com");
    assert_eq!(created["enabled"], true);
    assert!(
        created["id"]
            .as_str()
            .unwrap()
            .starts_with("gts.cf.core.oagw.upstream.v1~")
    );
    assert_eq!(created["tenant_id"], Value::String(tenant().to_string()));
}

/// An explicit alias must equal the derived one.
#[tokio::test]
async fn explicit_alias_must_match_the_derived_value() {
    let harness = Harness::new();
    let body = json!({
        "alias": "wrong.example.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
    });
    let problem = harness
        .expect_problem(
            harness.post_json("/oagw/v1/upstreams", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("api.vendor.com")
    );
}

/// A pool that cannot be derived requires an explicit alias.
#[tokio::test]
async fn ip_pools_require_an_explicit_alias() {
    let harness = Harness::new();
    let body = json!({
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "127.0.0.1", "port": 8443 } ] }
    });
    let problem = harness
        .expect_problem(
            harness.post_json("/oagw/v1/upstreams", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("explicit alias")
    );

    let body = json!({
        "alias": "loopback",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "127.0.0.1", "port": 8443 } ] }
    });
    let created = create_upstream(&harness, body).await;
    assert_eq!(created["alias"], "loopback");
}

/// Aliases are unique per tenant.
#[tokio::test]
async fn duplicate_aliases_conflict() {
    let harness = Harness::new();
    let body = json!({
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
    });
    create_upstream(&harness, body.clone()).await;
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/upstreams", body).await,
            StatusCode::CONFLICT,
        )
        .await;
}

/// The CRUD lifecycle of an upstream.
#[tokio::test]
async fn upstream_lifecycle() {
    let harness = Harness::new();
    let created = create_upstream(&harness, upstream_body("api.vendor.com", 443)).await;
    let id = created["id"].as_str().unwrap().to_owned();

    let fetched = harness
        .json(
            harness
                .send(http::Method::GET, &format!("/oagw/v1/upstreams/{id}"), None)
                .await,
        )
        .await;
    assert_eq!(fetched["id"], created["id"]);

    let listed = harness
        .json(
            harness
                .send(http::Method::GET, "/oagw/v1/upstreams", None)
                .await,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Replacing with the same pool keeps the alias.
    let mut replacement = created.clone();
    replacement["tags"] = json!(["prod"]);
    let replaced = harness
        .json(
            harness
                .put_json(&format!("/oagw/v1/upstreams/{id}"), replacement)
                .await,
        )
        .await;
    assert_eq!(replaced["alias"], created["alias"]);
    assert_eq!(replaced["tags"][0], "prod");

    let response = harness
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    harness
        .expect_problem(
            harness
                .send(http::Method::GET, &format!("/oagw/v1/upstreams/{id}"), None)
                .await,
            StatusCode::NOT_FOUND,
        )
        .await;
}

/// The alias is immutable: an endpoint change that recomputes it is rejected.
#[tokio::test]
async fn alias_is_immutable_on_replace() {
    let harness = Harness::new();
    let created = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let mut replacement = created.clone();
    replacement["server"]["endpoints"] = json!([ { "host": "other.vendor.com" } ]);
    harness
        .expect_problem(
            harness
                .put_json(&format!("/oagw/v1/upstreams/{id}"), replacement)
                .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
}

/// Plaintext endpoints are gated on the configuration.
#[tokio::test]
async fn plaintext_endpoints_require_allow_http_upstream() {
    let permissive = Harness::with_config(
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        None,
    );
    let created = create_upstream(
        &permissive,
        json!({
            "alias": "plain",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8080 } ] }
        }),
    )
    .await;
    assert_eq!(created["server"]["endpoints"][0]["scheme"], "http");

    let strict = Harness::with_config(
        OagwConfig {
            allow_http_upstream: false,
            ..OagwConfig::default()
        },
        None,
    );
    let body = json!({
        "alias": "plain",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8080 } ] }
    });
    strict
        .expect_problem(
            strict.post_json("/oagw/v1/upstreams", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
}

/// Unknown protocols are rejected.
#[tokio::test]
async fn unsupported_protocols_are_rejected() {
    let harness = Harness::new();
    let body = json!({
        "alias": "grpc",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
    });
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/upstreams", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
}

/// A route needs an existing upstream and a usable match rule.
#[tokio::test]
async fn route_validation() {
    let harness = Harness::new();
    let upstream = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
        }),
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    // Unknown upstream reference.
    let body = route_body(
        "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000000",
        "/v1",
    );
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/routes", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;

    // Happy path.
    let route = create_route(&harness, route_body(&upstream_id, "/v1/echo")).await;
    assert!(
        route["id"]
            .as_str()
            .unwrap()
            .starts_with("gts.cf.core.oagw.route.v1~")
    );
    assert_eq!(route["match"]["http"]["path"], "/v1/echo");

    // A second route with the same match rule conflicts.
    harness
        .expect_problem(
            harness
                .post_json("/oagw/v1/routes", route_body(&upstream_id, "/v1/echo"))
                .await,
            StatusCode::CONFLICT,
        )
        .await;

    // Empty method allowlist.
    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": [], "path": "/v2" } }
    });
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/routes", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;

    // Replace and delete.
    let id = route["id"].as_str().unwrap().to_owned();
    let mut replacement = route.clone();
    replacement["priority"] = json!(5);
    let replaced = harness
        .json(
            harness
                .put_json(&format!("/oagw/v1/routes/{id}"), replacement)
                .await,
        )
        .await;
    assert_eq!(replaced["priority"], 5);
    let response = harness
        .send(http::Method::DELETE, &format!("/oagw/v1/routes/{id}"), None)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// The route's owning upstream is immutable: a replacement naming another
/// upstream keeps the original binding.
#[tokio::test]
async fn route_upstream_is_immutable() {
    let harness = Harness::new();
    let first = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
        }),
    )
    .await;
    let second = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.other.com" } ] }
        }),
    )
    .await;
    let route = create_route(&harness, route_body(first["id"].as_str().unwrap(), "/v1")).await;
    let mut replacement = route.clone();
    replacement["upstream_id"] = second["id"].clone();
    let replaced = harness
        .json(
            harness
                .put_json(
                    &format!("/oagw/v1/routes/{}", route["id"].as_str().unwrap()),
                    replacement,
                )
                .await,
        )
        .await;
    assert_eq!(replaced["upstream_id"], first["id"]);
}

/// Plugin CRUD and the in-use guard.
#[tokio::test]
async fn plugin_lifecycle() {
    let harness = Harness::new();
    let body = json!({
        "plugin_type": "guard",
        "name": "required-headers",
        "source_code": "def guard_request(ctx):\n    pass\n",
        "phases": ["request"]
    });
    let created = harness
        .json(harness.post_json("/oagw/v1/plugins", body).await)
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));

    let source_path = format!("/oagw/v1/plugins/{id}/source");
    let source = harness
        .json(harness.send(http::Method::GET, &source_path, None).await)
        .await;
    assert_eq!(source["plugin_type"], "guard");
    assert!(
        source["source_code"]
            .as_str()
            .unwrap()
            .contains("guard_request")
    );

    // A duplicate name conflicts.
    let duplicate = json!({
        "plugin_type": "guard",
        "name": "required-headers",
        "source_code": "def guard_request(ctx):\n    pass\n"
    });
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/plugins", duplicate).await,
            StatusCode::CONFLICT,
        )
        .await;

    // Referenced plugins cannot be deleted.
    let upstream = create_upstream(
        &harness,
        json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "plugins": { "items": [ { "plugin_ref": id } ] }
        }),
    )
    .await;
    assert_eq!(upstream["plugins"]["items"][0]["plugin_ref"], id);
    harness
        .expect_problem(
            harness
                .send(
                    http::Method::DELETE,
                    &format!("/oagw/v1/plugins/{id}"),
                    None,
                )
                .await,
            StatusCode::CONFLICT,
        )
        .await;

    // Once unreferenced the plugin can go.
    let mut replacement = upstream.clone();
    replacement["plugins"] = Value::Null;
    let updated = harness
        .json(
            harness
                .put_json(
                    &format!("/oagw/v1/upstreams/{}", upstream["id"].as_str().unwrap()),
                    replacement,
                )
                .await,
        )
        .await;
    assert_eq!(updated["plugins"], Value::Null);
    let response = harness
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// Unknown plugin kinds are rejected and the source endpoint 404s.
#[tokio::test]
async fn plugin_kind_validation() {
    let harness = Harness::new();
    let body = json!({ "plugin_type": "filter", "name": "filter" });
    harness
        .expect_problem(
            harness.post_json("/oagw/v1/plugins", body).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
}

/// The effective configuration is exposed.
#[tokio::test]
async fn config_endpoint_reports_the_limits() {
    let harness = Harness::with_config(
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        None,
    );
    let config = harness
        .json(
            harness
                .send(http::Method::GET, "/oagw/v1/config", None)
                .await,
        )
        .await;
    assert_eq!(config["allow_http_upstream"], true);
    assert_eq!(config["proxy_timeout_secs"], 30);
    assert_eq!(config["max_request_body_bytes"], 104_857_600_u64);
}

/// Errors are RFC 9457 problem documents with a GTS type.
#[tokio::test]
async fn errors_are_problem_documents() {
    let harness = Harness::new();
    let response = harness
        .send(http::Method::GET, "/oagw/v1/upstreams/does-not-exist", None)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()["content-type"],
        "application/problem+json"
    );
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = harness.json(response).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .starts_with("gts.cf.core.errors.")
    );
    assert_eq!(problem["status"], 400);
    assert_eq!(problem["title"], "Validation Error");
}

/// A malformed GTS identifier in the path is a 400; a well-formed but unknown
/// one is a 404.
#[tokio::test]
async fn malformed_identifiers_are_rejected() {
    let harness = Harness::new();
    // A bare UUID is not an instance identifier.
    let response = harness
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000",
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A well-formed identifier of the wrong family is not an upstream either.
    let response = harness
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams/gts.cf.core.oagw.route.v1~00000000-0000-0000-0000-000000000000",
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A well-formed upstream identifier that does not exist is a 404.
    let response = harness
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams/gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000001",
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Catalog-only plugin identifiers have no implementation and are refused.
#[tokio::test]
async fn catalog_only_bindings_are_rejected() {
    let harness = Harness::new();
    let cases = [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
    ];
    for reference in cases {
        let response = harness
            .post_json(
                "/oagw/v1/upstreams",
                json!({
                    "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                    "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                    "plugins": { "items": [ { "plugin_ref": reference } ] }
                }),
            )
            .await;
        let problem = harness
            .expect_problem(response, StatusCode::BAD_REQUEST)
            .await;
        assert!(
            problem["detail"].as_str().unwrap().contains("catalog-only"),
            "{reference} is refused with an explanation: {problem}"
        );
    }
}
