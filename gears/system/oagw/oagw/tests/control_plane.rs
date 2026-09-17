// Created: 2026-09-03 by Constructor Tech
//! Control-plane integration tests.
//!
//! Exercises the real `full_router` surface: gear-relative paths, JSON
//! framing, validation rejections, conflict handling, the OData list
//! envelope and the RFC 9457 problem documents of the error catalogue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    alias_of, assert_problem, http_endpoint, https_endpoint, router, route_match_value, security_context,
    send_json, state, tenant, upstream_gts, upstream_payload,
};
use http::{HeaderMap, Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

/// Creates an upstream through the API and returns its document.
async fn create_upstream(app: axum::Router, endpoints: Value) -> Value {
    let (status, _, document) = send_create(app, endpoints).await;
    assert_eq!(status, StatusCode::CREATED, "document: {document}");
    document
}

async fn send_create(app: axum::Router, endpoints: Value) -> (StatusCode, HeaderMap, Value) {
    send_json(app, Method::POST, "/oagw/v1/upstreams", Some(upstream_payload(endpoints))).await
}

/// The canonical protocol identifier the API stores.
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

#[tokio::test]
async fn upstream_lifecycle_round_trips() {
    let app = router(state(true), security_context(tenant()));
    let endpoint = http_endpoint("api.example.com", 8443);

    let created = create_upstream(app.clone(), json!([endpoint])).await;
    let id = created["id"].as_str().expect("id").to_owned();
    let parsed = Uuid::parse_str(&id).expect("uuid");
    assert_eq!(created["tenant_id"], json!(app_tenant(&created)));
    assert_eq!(created["alias"], json!(alias_of(&endpoint)));
    assert_eq!(created["protocol"], PROTOCOL_HTTP);
    assert!(created["created_at"].is_u64());
    assert!(created["updated_at"].is_u64());

    // GET by bare UUID and by GTS identifier.
    let (status, _, document) =
        send_json(app.clone(), Method::GET, &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["id"].as_str().expect("id"), id.as_str());

    let (status, _, document) = send_json(
        app.clone(),
        Method::GET,
        &format!("/oagw/v1/upstreams/{}", upstream_gts(parsed)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["alias"], created["alias"]);

    // List returns the envelope with a single item.
    let (status, _, document) = send_json(app.clone(), Method::GET, "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["count"], 1);
    assert_eq!(document["items"][0]["id"].as_str().expect("id"), id.as_str());
    assert!(document["top"].is_u64());
    assert_eq!(document["skip"], 0);

    // Replace keeps the alias and the identity.
    let replacement = upstream_payload(json!([endpoint]));
    let (status, _, document) = send_json(
        app.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], created["alias"]);
    assert_eq!(document["created_at"], created["created_at"]);

    // Delete returns 204 and the resource disappears.
    let (status, _, body) = send_json(app.clone(), Method::DELETE, &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null());
    let (status, headers, document) =
        send_json(app, Method::GET, &format!("/oagw/v1/upstreams/{id}"), None).await;
    let kind = assert_problem(status, &headers, &document, 404);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
}

fn app_tenant(document: &Value) -> Uuid {
    Uuid::parse_str(document["tenant_id"].as_str().expect("tenant_id")).expect("uuid")
}

#[tokio::test]
async fn alias_conflicts_and_overrides_are_rejected() {
    let app = router(state(true), security_context(tenant()));
    let first = create_upstream(app.clone(), json!([http_endpoint("api.example.com", 8443)])).await;
    let alias = first["alias"].as_str().expect("alias").to_owned();

    // A second upstream deriving the same alias collides.
    let (status, headers, document) =
        send_create(app.clone(), json!([http_endpoint("api.example.com", 8443)])).await;
    let kind = assert_problem(status, &headers, &document, 409);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");

    // An explicit alias that contradicts the derived one is rejected.
    let (status, headers, document) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "alias": "other.example.com",
            "server": { "endpoints": [http_endpoint("api.example.com", 8443)] },
            "protocol": "http"
        })),
    )
    .await;
    assert_problem(status, &headers, &document, 400);

    // IP-based pools need an explicit alias.
    let (status, headers, document) = send_create(app.clone(), json!([http_endpoint("127.0.0.1", 9000)])).await;
    assert_problem(status, &headers, &document, 400);

    // A declared alias for an IP pool is accepted verbatim.
    let (status, _, document) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(serde_json::json!({
            "alias": "127-0-0-1.internal",
            "server": { "endpoints": [http_endpoint("127.0.0.1", 9000)] },
            "protocol": "http"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "document: {document}");
    assert_eq!(document["alias"], "127-0-0-1.internal");

    // The stored alias is untouched by the rejections.
    let (status, _, document) = send_json(app, Method::GET, "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    let aliases: Vec<&str> = document["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .collect();
    assert!(aliases.contains(&alias.as_str()), "aliases: {aliases:?}");
    assert!(aliases.contains(&"127-0-0-1.internal"));
}

#[tokio::test]
async fn endpoint_schemes_follow_the_configuration_switch() {
    let http_upstream = upstream_payload(json!([http_endpoint("svc.example.com", 8080)]));
    let https_upstream = upstream_payload(json!([https_endpoint("svc.example.com", 8443)]));

    // HTTPS is always accepted; `http` follows the flag.
    let allowed = router(state(true), security_context(tenant()));
    let (status, _, _) = send_json(allowed.clone(), Method::POST, "/oagw/v1/upstreams", Some(http_upstream.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send_json(allowed, Method::POST, "/oagw/v1/upstreams", Some(https_upstream.clone())).await;
    assert_eq!(status, StatusCode::CREATED);

    // The default posture is HTTPS-only.
    let denied = router(state(false), security_context(tenant()));
    let (status, headers, document) =
        send_json(denied.clone(), Method::POST, "/oagw/v1/upstreams", Some(http_upstream)).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"));
    let (status, _, _) = send_json(denied, Method::POST, "/oagw/v1/upstreams", Some(https_upstream)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn malformed_upstream_payloads_are_rejected() {
    let app = router(state(true), security_context(tenant()));

    let cases: Vec<(Value, &str)> = vec![
        (upstream_payload(json!([])), "empty endpoint pool"),
        (
            serde_json::json!({
                "server": { "endpoints": [http_endpoint("api.example.com", 8443)] },
                "protocol": "carrier-pigeon"
            }),
            "unsupported protocol",
        ),
        (
            serde_json::json!({
                "server": { "endpoints": [http_endpoint("bad_host!", 8443)] },
                "protocol": "http"
            }),
            "invalid hostname",
        ),
        (
            serde_json::json!({
                "server": { "endpoints": [http_endpoint("api.example.com", 8443)] },
                "protocol": "http",
                "tags": ["", "ok"]
            }),
            "empty tag",
        ),
        (
            serde_json::json!({
                "server": { "endpoints": [http_endpoint("api.example.com", 8443)] },
                "protocol": "http",
                "auth": { "type": "basic" }
            }),
            "unsupported auth plugin",
        ),
        (
            serde_json::json!({
                "server": { "endpoints": [http_endpoint("api.example.com", 8443)] },
                "protocol": "http",
                "auth": { "type": "apikey", "config": [] }
            }),
            "auth config is not an object",
        ),
    ];

    for (payload, label) in cases {
        let (status, headers, document) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case: {label} body: {document}");
        let kind = assert_problem(status, &headers, &document, 400);
        assert!(kind.ends_with("validation.error.v1"), "case: {label} type: {kind}");
    }
}

#[tokio::test]
async fn route_lifecycle_and_reference_protection() {
    let app = router(state(true), security_context(tenant()));
    let upstream = create_upstream(app.clone(), json!([http_endpoint("routes.example.com", 9000)])).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let upstream_uuid = Uuid::parse_str(&upstream_id).expect("uuid");

    let route_payload = json!({
        "upstream_id": upstream_uuid,
        "match": route_match_value(vec!["GET", "POST"], "/v1")
    });
    let (status, _, route) = send_json(app.clone(), Method::POST, "/oagw/v1/routes", Some(route_payload.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "route: {route}");
    let route_id = route["id"].as_str().expect("id").to_owned();
    assert_eq!(route["upstream_id"], json!(upstream_uuid));

    // A duplicate match rule on the same upstream collides.
    let (status, headers, document) =
        send_json(app.clone(), Method::POST, "/oagw/v1/routes", Some(route_payload.clone())).await;
    assert_problem(status, &headers, &document, 409);

    // Deleting the upstream is refused while the route exists.
    let (status, headers, document) = send_json(
        app.clone(),
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        None,
    )
    .await;
    assert_problem(status, &headers, &document, 409);

    // Replace keeps the upstream binding.
    let replacement = json!({
        "upstream_id": upstream_uuid,
        "enabled": false,
        "match": route_match_value(vec!["GET"], "/v2")
    });
    let (status, _, document) =
        send_json(app.clone(), Method::PUT, &format!("/oagw/v1/routes/{route_id}"), Some(replacement)).await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["upstream_id"], json!(upstream_uuid));
    assert_eq!(document["enabled"], false);
    assert_eq!(document["match"]["http"]["path"], "/v2");

    // Re-pointing a route at another upstream is refused.
    let other = create_upstream(app.clone(), json!([http_endpoint("other.example.com", 9000)])).await;
    let (status, headers, document) = send_json(
        app.clone(),
        Method::PUT,
        &format!("/oagw/v1/routes/{route_id}"),
        Some(json!({
            "upstream_id": other["id"],
            "match": route_match_value(vec!["GET"], "/v2")
        })),
    )
    .await;
    assert_problem(status, &headers, &document, 409);

    // The route can be deleted, and only then the upstream.
    let (status, _, _) = send_json(app.clone(), Method::DELETE, &format!("/oagw/v1/routes/{route_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send_json(app, Method::DELETE, &format!("/oagw/v1/upstreams/{upstream_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn routes_require_a_match_rule_and_a_live_upstream() {
    let app = router(state(true), security_context(tenant()));
    let upstream = create_upstream(app.clone(), json!([http_endpoint("live.example.com", 9000)])).await;

    let (status, headers, document) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({ "upstream_id": upstream["id"] })),
    )
    .await;
    assert_problem(status, &headers, &document, 400);

    let (status, _, document) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": Uuid::new_v4(),
            "match": route_match_value(vec!["GET"], "/")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "document: {document}");

    let (status, _, document) = send_json(
        app,
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": { "http": { "methods": ["OPTIONS"], "path": "/" } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "document: {document}");
}

#[tokio::test]
async fn plugin_definitions_refuse_deletion_while_referenced() {
    let app = router(state(true), security_context(tenant()));
    create_upstream(app.clone(), json!([http_endpoint("plugins.example.com", 9000)])).await;

    let plugin_payload = json!({
        "name": "my-guard",
        "plugin_type": "guard",
        "config": { "headers": ["x-tenant"] }
    });
    let (status, _, plugin) = send_json(app.clone(), Method::POST, "/oagw/v1/plugins", Some(plugin_payload.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "plugin: {plugin}");
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    // A duplicate name collides.
    let (status, headers, document) = send_json(app.clone(), Method::POST, "/oagw/v1/plugins", Some(plugin_payload)).await;
    assert_problem(status, &headers, &document, 409);

    // The source endpoint of a plugin is not part of this surface.
    let (status, _, _) = send_json(
        app.clone(),
        Method::GET,
        &format!("/oagw/v1/plugins/{plugin_id}/source"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Deleting an unreferenced plugin works.
    let (status, _, _) = send_json(app.clone(), Method::DELETE, &format!("/oagw/v1/plugins/{plugin_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Re-create it and bind it to a new upstream, which must block deletion.
    let (status, _, plugin) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/plugins",
        Some(json!({
            "name": "my-guard",
            "plugin_type": "guard",
            "config": { "headers": ["x-tenant"] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "plugin: {plugin}");
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();
    let bound = json!({
        "server": { "endpoints": [http_endpoint("plugins.example.com", 9001)] },
        "protocol": "http",
        "plugins": { "items": [ { "plugin_ref": plugin_id } ] }
    });
    let (status, _, document) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(bound)).await;
    assert_eq!(status, StatusCode::CREATED, "document: {document}");
    let bound_id = document["id"].as_str().expect("id").to_owned();

    let (status, headers, document) =
        send_json(app.clone(), Method::DELETE, &format!("/oagw/v1/plugins/{plugin_id}"), None).await;
    let kind = assert_problem(status, &headers, &document, 409);
    assert_eq!(kind, "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
    assert_eq!(
        document["referenced_by"]["upstreams"].as_array().map(Vec::len),
        Some(1)
    );

    // Removing the binding frees the plugin again.
    let (status, _, _) = send_json(app, Method::DELETE, &format!("/oagw/v1/upstreams/{bound_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn list_endpoints_page_filter_and_project() {
    let app = router(state(true), security_context(tenant()));
    for name in ["a.example.com", "b.example.com", "c.example.com"] {
        let (status, _, _) =
            send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(upstream_payload(json!([http_endpoint(name, 9001)])))).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, _, document) =
        send_json(app.clone(), Method::GET, "/oagw/v1/upstreams?$top=2&$skip=1", None).await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["count"], 2);
    assert_eq!(document["top"], 2);
    assert_eq!(document["skip"], 1);
    let aliases: Vec<&str> = document["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .collect();
    assert_eq!(aliases, vec!["b.example.com:9001", "c.example.com:9001"]);

    let (status, _, document) = send_json(
        app.clone(),
        Method::GET,
        "/oagw/v1/upstreams?$filter=alias%20eq%20%27a.example.com%3A9001%27",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["count"], 1);
    assert_eq!(document["items"][0]["alias"], "a.example.com:9001");

    let (status, _, document) =
        send_json(app.clone(), Method::GET, "/oagw/v1/upstreams?$select=alias", None).await;
    assert_eq!(status, StatusCode::OK);
    let item = &document["items"][0];
    assert!(item.get("id").is_none(), "projection kept id: {item}");
    assert!(item["alias"].is_string());

    let (status, headers, document) = send_json(app, Method::GET, "/oagw/v1/upstreams?$top=101", None).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"));
}

#[tokio::test]
async fn tenants_are_isolated() {
    let owner = router(state(true), security_context(tenant()));
    let upstream = create_upstream(owner.clone(), json!([http_endpoint("private.example.com", 9002)])).await;
    let id = upstream["id"].as_str().expect("id").to_owned();

    let other = router(state(true), security_context(tenant()));
    for method in [Method::GET, Method::DELETE] {
        let (status, _, document) = send_json(
            other.clone(),
            method.clone(),
            &format!("/oagw/v1/upstreams/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "method: {method} body: {document}");
    }
    let (status, _, document) = send_json(
        other.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(upstream_payload(json!([http_endpoint("private.example.com", 9002)]))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "document: {document}");
    let (status, _, document) = send_json(other, Method::GET, "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["count"], 0);
    drop(owner);
}

#[tokio::test]
async fn malformed_identifiers_are_rejected() {
    let app = router(state(true), security_context(tenant()));
    for uri in [
        "/oagw/v1/upstreams/not-a-uuid",
        "/oagw/v1/routes/not-a-uuid",
        "/oagw/v1/plugins/not-a-uuid",
    ] {
        let (status, headers, document) = send_json(app.clone(), Method::GET, uri, None).await;
        let kind = assert_problem(status, &headers, &document, 400);
        assert!(kind.ends_with("validation.error.v1"), "uri: {uri} type: {kind}");
    }
    let (status, _, _) = send_json(app, Method::GET, &format!("/oagw/v1/routes/{}", Uuid::new_v4()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Alias update matrix (DESIGN.md "Alias Update Behavior")
// ---------------------------------------------------------------------------

/// Creates an upstream from a raw endpoint array and returns its document.
async fn seed(app: axum::Router, endpoints: Value) -> Value {
    let (status, _, document) = send_json(
        app,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_payload(endpoints)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "document: {document}");
    document
}

/// Replaces an upstream, optionally restating its alias.
async fn replace(
    app: axum::Router,
    id: &str,
    alias: Option<&str>,
    endpoints: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut payload = upstream_payload(endpoints);
    if let Some(alias) = alias {
        payload["alias"] = json!(alias);
    }
    send_json(app, Method::PUT, &format!("/oagw/v1/upstreams/{id}"), Some(payload)).await
}

#[tokio::test]
async fn alias_update_to_a_pool_with_the_same_derived_alias_is_allowed() {
    // Derivable -> Derivable, the recomputed alias equals the stored one.
    let app = router(state(true), security_context(tenant()));
    let created = seed(
        app.clone(),
        json!([https_endpoint("us.vendor.com", 443), https_endpoint("eu.vendor.com", 443)]),
    )
    .await;
    let alias = created["alias"].as_str().expect("alias").to_owned();
    assert_eq!(alias, "vendor.com");

    let (status, _, document) = send_json(
        app.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id")),
        Some(upstream_payload(json!([
            https_endpoint("ap.vendor.com", 443),
            https_endpoint("sa.vendor.com", 443)
        ]))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], json!(alias));
}

#[tokio::test]
async fn alias_update_derivable_change_rejects_a_new_alias() {
    // Derivable -> Derivable, the new endpoints derive a different alias.
    let app = router(state(true), security_context(tenant()));
    let created = seed(app.clone(), json!([https_endpoint("api.example.com", 8443)])).await;

    let (status, headers, document) = send_json(
        app.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id")),
        Some(upstream_payload(json!([https_endpoint("api.other.com", 8443)]))),
    )
    .await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");

    // The stored upstream is unchanged by the rejected replacement.
    let (status, _, document) =
        send_json(app, Method::GET, &format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id")), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["server"]["endpoints"][0]["host"], "api.example.com");
}

#[tokio::test]
async fn alias_update_from_a_hostname_to_an_ip_is_rejected() {
    // Derivable -> Non-derivable is rejected, with or without an explicit alias.
    let app = router(state(true), security_context(tenant()));
    let created = seed(app.clone(), json!([https_endpoint("api.example.com", 443)])).await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, document) = replace(app.clone(), &id, None, json!([https_endpoint("10.0.1.1", 443)])).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");

    let (status, _, _) =
        replace(app.clone(), &id, Some("api.example.com"), json!([https_endpoint("10.0.1.1", 443)])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an explicit alias must not unlock the change");

    // The stored upstream is untouched by both rejected replacements.
    let (status, _, document) = send_json(app, Method::GET, &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["server"]["endpoints"][0]["host"], "api.example.com");
}

#[tokio::test]
async fn alias_update_ip_to_ip_retains_the_alias() {
    // Non-derivable -> Non-derivable with no alias supplied keeps the stored alias.
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "ip-pool.internal",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    assert_eq!(created["alias"], "ip-pool.internal");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, _, document) =
        replace(app.clone(), &id, None, json!([https_endpoint("10.0.1.2", 443)])).await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], "ip-pool.internal");
    assert_eq!(document["server"]["endpoints"][0]["host"], "10.0.1.2");
}

#[tokio::test]
async fn alias_update_ip_to_ip_rejects_a_differing_alias() {
    // Non-derivable -> Non-derivable with a different alias is not accepted.
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "ip-pool.internal",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, document) =
        replace(app.clone(), &id, Some("renamed.internal"), json!([https_endpoint("10.0.1.1", 443)])).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");

    let (status, _, document) = send_json(app, Method::GET, &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["alias"], "ip-pool.internal");
}

#[tokio::test]
async fn alias_update_from_an_ip_to_the_derived_hostname_is_allowed() {
    // Non-derivable -> Derivable, the derived alias equals the stored one.
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "migrated.example.com",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, _, document) =
        replace(app.clone(), &id, None, json!([https_endpoint("migrated.example.com", 443)])).await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], "migrated.example.com");
}

#[tokio::test]
async fn alias_update_from_an_ip_to_a_renamed_hostname_is_rejected() {
    // Non-derivable -> Derivable, the derived alias differs from the stored one.
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "migrated.example.com",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, document) =
        replace(app.clone(), &id, None, json!([https_endpoint("elsewhere.example.com", 443)])).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");
}

#[tokio::test]
async fn alias_update_rejects_an_explicit_alias_override() {
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "ip-pool.internal",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // An override is refused whether the endpoints change or not.
    for endpoints in [
        json!([https_endpoint("10.0.1.1", 443)]),
        json!([https_endpoint("10.0.1.2", 443)]),
    ] {
        let (status, headers, document) =
            replace(app.clone(), &id, Some("renamed.internal"), endpoints).await;
        let kind = assert_problem(status, &headers, &document, 400);
        assert!(kind.ends_with("validation.error.v1"), "type: {kind}");
    }

    let derived = seed(app.clone(), json!([https_endpoint("api.example.com", 443)])).await;
    let (status, _, _) = replace(
        app.clone(),
        derived["id"].as_str().expect("id"),
        Some("api.example.com"),
        json!([https_endpoint("api.example.com", 443)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "restating the alias verbatim is a no-op");
}

#[tokio::test]
async fn alias_update_tolerates_an_exact_match_alias_on_a_no_op_change() {
    // No endpoint change: the exact-match alias is tolerated as an idempotent no-op.
    let app = router(state(true), security_context(tenant()));
    let created = seed_with_alias(
        app.clone(),
        "ip-pool.internal",
        json!([https_endpoint("10.0.1.1", 443)]),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, _, document) =
        replace(app.clone(), &id, Some("ip-pool.internal"), json!([https_endpoint("10.0.1.1", 443)])).await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], "ip-pool.internal");
    assert_eq!(document["created_at"], created["created_at"]);

    // The same tolerance applies to a derivable pool.
    let derived = seed(app.clone(), json!([https_endpoint("api.example.com", 443)])).await;
    let (status, _, document) = replace(
        app,
        derived["id"].as_str().expect("id"),
        Some("api.example.com"),
        json!([https_endpoint("api.example.com", 443)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["alias"], "api.example.com");
}

/// Creates an upstream with an explicit alias and returns its document.
async fn seed_with_alias(app: axum::Router, alias: &str, endpoints: Value) -> Value {
    let payload = json!({
        "alias": alias,
        "server": { "endpoints": endpoints },
        "protocol": "http"
    });
    let (status, _, document) = send_json(app, Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "document: {document}");
    document
}

#[tokio::test]
async fn alias_derivation_uses_the_endpoint_pool() {
    let app = router(state(true), security_context(tenant()));
    let created = create_upstream(app, json!([http_endpoint("plain.example.com", 443)])).await;
    assert_eq!(created["alias"], json!(alias_of(&http_endpoint("plain.example.com", 443))));
}

#[tokio::test]
async fn protocol_selectors_are_canonicalised() {
    let app = router(state(true), security_context(tenant()));
    for (index, spelling) in ["http", "HTTP", PROTOCOL_HTTP].iter().enumerate() {
        let payload = json!({
            "server": { "endpoints": [http_endpoint("canonical.example.com", 9100 + index as u16)] },
            "protocol": spelling
        });
        let (status, _, document) =
            send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED, "spelling: {spelling} body: {document}");
        assert_eq!(document["protocol"], PROTOCOL_HTTP);
    }
    let (status, _, document) = send_json(
        app,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(json!({
            "server": { "endpoints": [http_endpoint("canonical.example.com", 9101)] },
            "protocol": "carrier-pigeon"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "document: {document}");
}

#[tokio::test]
async fn cors_credentials_cannot_be_combined_with_a_wildcard_origin() {
    // ADR/0004 "Confirmation": rejected at validation time, not on the data plane.
    let app = router(state(true), security_context(tenant()));
    let wildcard = json!({
        "server": { "endpoints": [http_endpoint("cors.example.com", 9200)] },
        "protocol": "http",
        "cors": { "enabled": true, "allowed_origins": ["*"], "allow_credentials": true }
    });
    let (status, headers, document) =
        send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(wildcard)).await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");

    let specific = json!({
        "server": { "endpoints": [http_endpoint("cors.example.com", 9201)] },
        "protocol": "http",
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allow_credentials": true
        }
    });
    let (status, _, _) = send_json(app.clone(), Method::POST, "/oagw/v1/upstreams", Some(specific)).await;
    assert_eq!(status, StatusCode::CREATED);

    let upstream = create_upstream(app.clone(), json!([http_endpoint("cors.example.com", 9202)])).await;
    let (status, headers, document) = send_json(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/"),
            "cors": { "enabled": true, "allowed_origins": ["*"], "allow_credentials": true }
        })),
    )
    .await;
    let kind = assert_problem(status, &headers, &document, 400);
    assert!(kind.ends_with("validation.error.v1"), "type: {kind}");

    // The same origin list without credentials is accepted.
    let (status, _, _) = send_json(
        app,
        Method::POST,
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream["id"],
            "match": route_match_value(vec!["GET"], "/public"),
            "cors": { "enabled": true, "allowed_origins": ["*"] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}
