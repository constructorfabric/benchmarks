//! Management API: upstreams, routes, plugins and list queries.

mod common;

use common::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creates_an_upstream_with_a_derived_alias() {
    let mut app = app();
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com"}]},
                "auth": {"type": oagw::gts::auth_plugin::NOOP}
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "api.vendor.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "https");
    assert_eq!(body["auth"]["sharing"], "private");
    // Server-assigned fields are not echoed back.
    assert!(body.get("tenant_id").is_none());
    assert!(body.get("created_at").is_none());
}

#[tokio::test]
async fn derives_the_common_suffix_of_a_hostname_pool() {
    let mut app = app();
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "https", "host": "us.vendor.com"},
                    {"scheme": "https", "host": "eu.vendor.com"}
                ]},
                "auth": {"type": oagw::gts::auth_plugin::NOOP}
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "vendor.com");
}

#[tokio::test]
async fn derives_a_port_suffix_for_a_non_standard_port() {
    let mut app = app();
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "protocol": oagw::gts::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 8443}]},
                "auth": {"type": oagw::gts::auth_plugin::NOOP}
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "api.vendor.com:8443");
}

#[tokio::test]
async fn an_ip_endpoint_requires_an_explicit_alias() {
    let mut app = app();
    // No alias: IP endpoints do not imply one.
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body("127.0.0.1", 8080)),
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");

    // With an explicit alias it is accepted.
    let mut explicit = upstream_body("127.0.0.1", 8080);
    explicit["alias"] = json!("loopback");
    let (status, body) = app.send("POST", "/oagw/v1/upstreams", Some(explicit)).await;
    assert_eq!(status, http::StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "loopback");
}

#[tokio::test]
async fn a_mismatched_alias_is_rejected_for_a_derivable_pool() {
    let mut app = app();
    let (status, body) = app
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "something-else",
                "protocol": oagw::gts::PROTOCOL_HTTP,
                "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com"}]},
                "auth": {"type": oagw::gts::auth_plugin::NOOP}
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], oagw::gts::errors::VALIDATION_ERROR);
}

#[tokio::test]
async fn supplying_the_derived_alias_is_idempotent() {
    let mut app = app();
    let mut body = upstream_body("api.vendor.com", 443);
    body["server"]["endpoints"][0] = json!({"scheme": "https", "host": "api.vendor.com"});
    body["alias"] = json!("api.vendor.com");
    let (status, created) = app.send("POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    assert_eq!(created["alias"], "api.vendor.com");
}

#[tokio::test]
async fn a_duplicate_alias_conflicts() {
    let mut app = app();
    let body = upstream_body("api.vendor.com", 443);
    let (first, _) = app
        .send("POST", "/oagw/v1/upstreams", Some(body.clone()))
        .await;
    assert_eq!(first, http::StatusCode::CREATED);
    let (second, err) = app.send("POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(second, http::StatusCode::CONFLICT, "{err}");
}

#[tokio::test]
async fn validates_the_alias_pattern() {
    let mut app = app();
    let mut body = upstream_body("10.0.0.1", 443);
    body["alias"] = json!("-not-a-valid-alias-");
    let (status, err) = app.send("POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{err}");
}

#[tokio::test]
async fn rejects_an_unknown_scheme_and_an_empty_endpoint_list() {
    let mut app = app();
    let bad_scheme = json!({
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "ftp", "host": "api.vendor.com"}]}
    });
    let (status, _) = app
        .send("POST", "/oagw/v1/upstreams", Some(bad_scheme))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);

    let empty = json!({
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": []}
    });
    let (status, _) = app.send("POST", "/oagw/v1/upstreams", Some(empty)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_a_scheme_outside_the_documented_set_even_when_http_is_allowed() {
    let mut app = app();
    // `http` is a legal scheme here (see the note on `allow_http_upstream`);
    // `ws` never was.
    let body = json!({
        "alias": "ws-upstream",
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "ws", "host": "api.vendor.com"}]}
    });
    let (status, _) = app.send("POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accepts_the_http_scheme() {
    let mut app = app();
    let body = json!({
        "protocol": oagw::gts::PROTOCOL_HTTP,
        "server": {"endpoints": [{"scheme": "http", "host": "api.vendor.com", "port": 80}]},
        "auth": {"type": oagw::gts::auth_plugin::NOOP}
    });
    let (status, created) = app.send("POST", "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    assert_eq!(created["server"]["endpoints"][0]["scheme"], "http");
    // Port 80 is standard for `http`, so it does not appear in the alias.
    assert_eq!(created["alias"], "api.vendor.com");
}

#[tokio::test]
async fn reads_lists_and_replaces_an_upstream() {
    let mut app = app();
    let id = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;

    let (status, got) = app
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::OK, "{got}");
    assert_eq!(got["id"], id.as_str());

    let (status, list) = app.send("GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, http::StatusCode::OK, "{list}");
    assert_eq!(list["count"], 1);
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["id"], id.as_str());

    let mut replacement = upstream_body("api.vendor.com", 443);
    replacement["alias"] = json!("api.vendor.com");
    replacement["enabled"] = json!(false);
    let (status, replaced) = app
        .send(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(replacement),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], false);
}

#[tokio::test]
async fn deletes_an_upstream() {
    let mut app = app();
    let id = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    let (status, _) = app
        .send("DELETE", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    let (status, _) = app
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_upstream_is_a_404() {
    let mut app = app();
    let (status, body) = app
        .send(
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-00000000dead",
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{body}");
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creates_and_reads_a_route() {
    let mut app = app();
    let upstream = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    let id = app.create_route(route_body(&upstream, "/v1")).await;

    let (status, got) = app
        .send("GET", &format!("/oagw/v1/routes/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::OK, "{got}");
    assert_eq!(got["match"]["http"]["path"], "/v1");
    assert_eq!(got["upstream_id"], upstream.as_str());

    let (status, list) = app.send("GET", "/oagw/v1/routes", None).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(list["count"], 1);
}

#[tokio::test]
async fn a_route_must_reference_an_upstream_of_the_calling_tenant() {
    let mut app = app();
    let body = route_body("00000000-0000-0000-0000-00000000dead", "/v1");
    let (status, err) = app.send("POST", "/oagw/v1/routes", Some(body)).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{err}");
}

#[tokio::test]
async fn a_duplicate_match_rule_conflicts() {
    let mut app = app();
    let upstream = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    let route = json!({
        "upstream_id": upstream,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}}
    });
    let (first, _) = app
        .send("POST", "/oagw/v1/routes", Some(route.clone()))
        .await;
    assert_eq!(first, http::StatusCode::CREATED);
    let (second, err) = app.send("POST", "/oagw/v1/routes", Some(route)).await;
    assert_eq!(second, http::StatusCode::CONFLICT, "{err}");
}

#[tokio::test]
async fn a_route_requires_exactly_one_match_protocol() {
    let mut app = app();
    let upstream = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;

    let none = json!({"upstream_id": upstream, "match": {}});
    let (status, _) = app.send("POST", "/oagw/v1/routes", Some(none)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);

    let both = json!({
        "upstream_id": upstream,
        "match": {
            "http": {"methods": ["GET"], "path": "/v1"},
            "grpc": {"service": "foo.v1.Svc", "method": "Get"}
        }
    });
    let (status, _) = app.send("POST", "/oagw/v1/routes", Some(both)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn replaces_and_deletes_a_route() {
    let mut app = app();
    let upstream = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    let id = app.create_route(route_body(&upstream, "/v1")).await;

    let replacement = json!({
        "upstream_id": upstream,
        "match": {"http": {"methods": ["GET"], "path": "/v2"}},
        "enabled": false
    });
    let (status, replaced) = app
        .send("PUT", &format!("/oagw/v1/routes/{id}"), Some(replacement))
        .await;
    assert_eq!(status, http::StatusCode::OK, "{replaced}");
    assert_eq!(replaced["match"]["http"]["path"], "/v2");
    assert_eq!(replaced["enabled"], false);

    let (status, _) = app
        .send("DELETE", &format!("/oagw/v1/routes/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    let (status, _) = app
        .send("GET", &format!("/oagw/v1/routes/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registers_reads_and_deletes_a_custom_plugin() {
    let mut app = app();
    let (status, created) = app
        .send(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "name": "tenant-header",
                "plugin_type": "transform",
                "phases": ["request"],
                "source_code": "def on_request(ctx): pass",
                "config_schema": {"type": "object"}
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, got) = app
        .send("GET", &format!("/oagw/v1/plugins/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::OK, "{got}");
    assert_eq!(got["name"], "tenant-header");

    // Plugins are immutable: there is no way to replace one.
    let (status, _) = app
        .send(
            "PUT",
            &format!("/oagw/v1/plugins/{id}"),
            Some(created.clone()),
        )
        .await;
    assert_eq!(status, http::StatusCode::METHOD_NOT_ALLOWED);

    let (status, source) = app
        .send("GET", &format!("/oagw/v1/plugins/{id}/source"), None)
        .await;
    assert_eq!(status, http::StatusCode::OK, "{source}");
    assert_eq!(source["source"], "def on_request(ctx): pass");

    let (status, _) = app
        .send("DELETE", &format!("/oagw/v1/plugins/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_a_bound_plugin_conflicts() {
    let mut app = app();
    let (status, plugin) = app
        .send(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "name": "tenant-guard",
                "plugin_type": "guard",
                "phases": ["request"]
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{plugin}");
    let id = plugin["id"].as_str().expect("id").to_owned();

    let upstream = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    let route = json!({
        "upstream_id": upstream,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        "plugins": {"items": [id]}
    });
    let (status, created) = app.send("POST", "/oagw/v1/routes", Some(route)).await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");

    let (status, err) = app
        .send("DELETE", &format!("/oagw/v1/plugins/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{err}");
    assert_eq!(err["type"], oagw::gts::errors::PLUGIN_IN_USE);
    assert!(!err["referenced_by"].is_null(), "{err}");
}

// ---------------------------------------------------------------------------
// Tenant scoping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn management_resources_are_invisible_across_tenants() {
    let mut app = app();
    let id = app
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;

    // The same routes, seen from a tenant that owns nothing.
    let mut other = App::new(
        MockResolver::default_hierarchy(),
        context_for(TENANT_OTHER),
        test_config(),
    );
    let (status, _) = other
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    let (status, list) = other.send("GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(list["count"], 0);
}

// ---------------------------------------------------------------------------
// List queries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_queries_filter_project_sort_and_page() {
    let mut app = app();
    for (host, enabled) in [
        ("a.vendor.com", true),
        ("b.vendor.com", false),
        ("c.vendor.com", true),
    ] {
        let mut body = upstream_body(host, 443);
        body["alias"] = json!(host);
        body["enabled"] = json!(enabled);
        app.create_upstream(body).await;
    }

    // $filter
    let (_, list) = app
        .send(
            "GET",
            "/oagw/v1/upstreams?$filter=enabled%20eq%20true",
            None,
        )
        .await;
    assert_eq!(list["count"], 2, "{list}");

    // $orderby + $top + $skip
    let (_, list) = app
        .send(
            "GET",
            "/oagw/v1/upstreams?$orderby=alias%20desc&$top=2&$skip=1",
            None,
        )
        .await;
    assert_eq!(list["count"], 2);
    assert_eq!(list["total"], 3);
    assert_eq!(list["items"][0]["alias"], "b.vendor.com");
    assert_eq!(list["items"][1]["alias"], "a.vendor.com");

    // $select
    let (_, list) = app
        .send("GET", "/oagw/v1/upstreams?$select=alias", None)
        .await;
    let item = &list["items"][0];
    assert!(!item["alias"].is_null(), "{item}");
    assert!(item.get("server").is_none(), "{item}");

    // Unknown option.
    let (status, _) = app
        .send("GET", "/oagw/v1/upstreams?$count=true", None)
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);

    // Out-of-range page size.
    let (status, _) = app.send("GET", "/oagw/v1/upstreams?$top=101", None).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);

    // Unsupported operator.
    let (status, _) = app
        .send(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20contains%20%27a%27",
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}
