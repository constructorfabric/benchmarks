//! Management-API integration tests (PRD `cpt-cf-oagw-fr-upstream-mgmt`,
//! `…route-mgmt`, `…plugin-mgmt`).
//!
//! Every test goes through the real axum router built by
//! [`common::harness`], so the assertions cover the wire contract: status
//! codes, `Location` headers, the JSON field names of
//! `docs/schemas/upstream.v1.schema.json` / `route.v1.schema.json`, and the
//! GTS problem-type ids of the DESIGN.md error table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use serde_json::json;

use common::{
    create_plugin, create_route, create_upstream, get_upstream, http_upstream, inline_guard_plugin,
    plugin_chain, put_upstream, token_bucket,
};

/// The upstream GTS type id every `gts_id` must carry.
const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";

#[tokio::test]
async fn create_upstream_returns_201_location_and_round_trips() {
    let h = common::harness();
    let (status, body, headers) = h
        .json(
            axum::http::Method::POST,
            "/oagw/v1/upstreams",
            &[],
            Some(json!({
                "enabled": true,
                "alias": "my-service",
                "tags": ["llm", "prod"],
                "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9", "port": 8443}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": token_bucket(5, 20, "tenant")
            })),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().expect("id").to_owned();
    assert!(
        headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|location| location == format!("/oagw/v1/upstreams/{id}")),
        "expected a Location header naming the new resource, got {headers:?}"
    );
    assert_eq!(body["alias"], "my-service");
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["alias_derived"], json!(false));
    assert_eq!(body["tags"], json!(["llm", "prod"]), "tags round-trip");
    assert_eq!(body["server"]["endpoints"][0]["host"], "10.0.0.9");
    assert_eq!(body["server"]["endpoints"][0]["port"], json!(8443));
    assert_eq!(
        body["gts_id"].as_str().unwrap().strip_suffix(&id),
        Some(UPSTREAM_TYPE),
        "{}",
        body["gts_id"]
    );
    assert_eq!(
        body["rate_limit"]["burst"]["capacity"],
        json!(20),
        "{}",
        body["rate_limit"]
    );

    // Round trip: GET by id returns the same resource.
    let (status, fetched) = get_upstream(&h, &id).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched, body);
}

#[tokio::test]
async fn create_upstream_rejects_a_missing_server_block() {
    let h = common::harness();
    let (status, body) = create_upstream(&h, json!({"alias": "no-endpoints"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["status"], json!(400));
    assert_eq!(body["violations"][0]["field"], "server.endpoints");
}

#[tokio::test]
async fn create_upstream_requires_an_explicit_alias_for_ip_endpoints() {
    let h = common::harness();
    let (status, body) = create_upstream(
        &h,
        json!({
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "alias");
    let detail = body["violations"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("alias"),
        "the violation must explain the alias rule: {detail}"
    );
}

#[tokio::test]
async fn create_upstream_rejects_a_malformed_alias_and_tags() {
    let h = common::harness();
    for alias in ["-bad", "bad-", "bad alias", ""] {
        let (status, body) =
            create_upstream(&h, http_upstream("10.0.0.9", 8443, Some(alias))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "alias `{alias}`: {body}");
        assert_eq!(body["violations"][0]["field"], "alias", "{body}");
    }
    let (status, body) = create_upstream(
        &h,
        json!({
            "alias": "ok-alias",
            "tags": ["Not_A_Tag"],
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "tags");
    assert!(
        body["violations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("^[a-z0-9_-]+$"),
        "{}",
        body
    );
}

#[tokio::test]
async fn hostname_endpoints_always_derive_the_alias() {
    let h = common::harness();
    // A differing alias is rejected…
    let (status, body) = create_upstream(
        &h,
        json!({
            "alias": "my-service",
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "alias");
    // …the derived value is tolerated as a no-op…
    let (status, body) = create_upstream(
        &h,
        json!({
            "alias": "api.openai.com",
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["alias_derived"], json!(true));
    // …and the derived alias carries the port when it is non-standard.
    let (status, body) = create_upstream(
        &h,
        json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 8443}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "api.openai.com:8443");
}

#[tokio::test]
async fn multiple_hostnames_derive_the_common_suffix() {
    let h = common::harness();
    let (status, body) = create_upstream(
        &h,
        json!({
            "server": {"endpoints": [
                {"scheme": "https", "host": "us.vendor.com"},
                {"scheme": "https", "host": "eu.vendor.com"}
            ]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "vendor.com");

    // A bare public suffix is not derivable: an explicit alias is required.
    let (status, body) = create_upstream(
        &h,
        json!({
            "server": {"endpoints": [
                {"scheme": "https", "host": "foo.co.uk"},
                {"scheme": "https", "host": "bar.co.uk"}
            ]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "alias");
}

#[tokio::test]
async fn upstream_endpoints_are_validated() {
    let h = common::harness();
    // A zero port is rejected.
    let (status, body) = create_upstream(&h, http_upstream("10.0.0.9", 0, Some("zero"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "server.endpoints[0].port");

    // A malformed host is rejected.
    let (status, body) = create_upstream(&h, http_upstream("bad host", 80, Some("bad-host"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "server.endpoints[0].host");

    // Two identical endpoints are rejected.
    let (status, body) = create_upstream(
        &h,
        json!({
            "alias": "dupe",
            "server": {"endpoints": [
                {"scheme": "https", "host": "a.vendor.com"},
                {"scheme": "https", "host": "A.vendor.com"}
            ]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "server.endpoints");

    // Endpoints of an upstream must share one scheme.
    let (status, body) = create_upstream(
        &h,
        json!({
            "alias": "mixed",
            "server": {"endpoints": [
                {"scheme": "https", "host": "a.vendor.com"},
                {"scheme": "http", "host": "b.vendor.com"}
            ]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["field"], "server.endpoints");
}

#[tokio::test]
async fn endpoint_schemes_round_trip_with_their_default_ports() {
    let h = common::harness();
    for (scheme, default_port) in [
        ("https", 443),
        ("http", 80),
        ("wss", 443),
        ("wt", 443),
        ("grpc", 443),
    ] {
        let alias = format!("scheme-{scheme}");
        let (status, body) = create_upstream(
            &h,
            json!({
                "alias": alias,
                "server": {"endpoints": [{"scheme": scheme, "host": "10.0.0.9"}]}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["server"]["endpoints"][0]["scheme"], scheme, "{body}");
        assert_eq!(
            body["server"]["endpoints"][0]["port"],
            json!(default_port),
            "scheme {scheme} defaults to port {default_port}"
        );
    }
}

#[tokio::test]
async fn upstream_list_get_and_404() {
    let h = common::harness();
    let (status, first) = create_upstream(&h, http_upstream("10.0.0.1", 8001, Some("one"))).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, second) = create_upstream(&h, http_upstream("10.0.0.2", 8002, Some("two"))).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");

    let (status, list, _) = h
        .json(axum::http::Method::GET, "/oagw/v1/upstreams", &[], None)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["total"], json!(2));
    assert_eq!(list["items"].as_array().map(Vec::len), Some(2));

    let id = first["id"].as_str().unwrap().to_owned();
    let (status, body) = get_upstream(&h, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], json!(id));

    // A GTS instance id is accepted in place of a bare UUID.
    let gts_id = format!("{UPSTREAM_TYPE}{id}");
    let (status, body) = get_upstream(&h, &gts_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    let (status, problem) = get_upstream(&h, "00000000-0000-0000-0000-000000000001").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
    assert_eq!(problem["status"], json!(404));
    let _ = second;
}

#[tokio::test]
async fn put_upstream_is_a_full_replacement() {
    let h = common::harness();
    let (status, created) = create_upstream(&h, json!({
        "alias": "replace-me",
        "tags": ["before"],
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9", "port": 9000}]},
        "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]}
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created["cors"].is_object(), "{}", created);
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, replaced) = put_upstream(
        &h,
        &id,
        json!({
            "enabled": false,
            "alias": "replace-me",
            "tags": ["after"],
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9", "port": 9001}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["id"], json!(id), "the id never changes");
    assert_eq!(replaced["enabled"], json!(false));
    assert_eq!(replaced["tags"], json!(["after"]));
    assert_eq!(replaced["server"]["endpoints"][0]["port"], json!(9001));
    // Omitted optional blocks are cleared by a full replacement.
    assert!(replaced["cors"].is_null(), "{}", replaced);

    let (status, fetched) = get_upstream(&h, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, replaced);
}

#[tokio::test]
async fn put_upstream_rejects_an_alias_change() {
    let h = common::harness();
    let (status, created) =
        create_upstream(&h, http_upstream("10.0.0.9", 8000, Some("stable"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, body) =
        put_upstream(&h, &id, http_upstream("10.0.0.9", 8000, Some("renamed"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.alias.immutable.v1"
    );
    // The alias is the routing key: nothing was written.
    let (status, fetched) = get_upstream(&h, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["alias"], "stable");
}

#[tokio::test]
async fn a_duplicated_alias_is_a_409_conflict() {
    let h = common::harness();
    let (status, first) =
        create_upstream(&h, http_upstream("10.0.0.1", 8001, Some("shared"))).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, problem) =
        create_upstream(&h, http_upstream("10.0.0.2", 8002, Some("shared"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1"
    );
    assert_eq!(problem["invalid_value"], "shared");
}

#[tokio::test]
async fn delete_upstream_returns_204_then_404_and_cascades_routes() {
    let h = common::harness();
    let (status, created) =
        create_upstream(&h, http_upstream("10.0.0.9", 8000, Some("doomed"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    let (status, route) = create_route(&h, json!({
        "enabled": true,
        "upstream_id": id,
        "match": {"http": {"methods": ["GET"], "path": "/v1/thing", "path_suffix_mode": "append"}}
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let route_id = route["id"].as_str().unwrap().to_owned();

    let (status, body, _) = h
        .json(
            axum::http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, problem) = get_upstream(&h, &id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
    // `oagw_route.upstream_id` is `ON DELETE CASCADE`.
    let (status, problem, _) = h
        .json(
            axum::http::Method::GET,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");

    // Deleting again is a 404, not a 204.
    let (status, problem, _) = h
        .json(
            axum::http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn routes_crud_and_the_upstream_id_is_immutable() {
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, http_upstream("10.0.0.9", 8000, Some("routed"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let (status, route) = create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "tags": ["chat"],
            "match": {"http": {"methods": ["POST", "GET"], "path": "/v1/chat",
                               "query_allowlist": ["model"], "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    assert_eq!(route["upstream_id"], json!(upstream_id));
    assert_eq!(
        route["match"]["http"]["path_suffix_mode"],
        json!("append"),
        "`append` is the documented default"
    );
    assert_eq!(route["match"]["http"]["methods"], json!(["POST", "GET"]));
    assert_eq!(
        route["gts_id"]
            .as_str()
            .unwrap()
            .strip_suffix(route["id"].as_str().unwrap()),
        Some(ROUTE_TYPE)
    );
    let route_id = route["id"].as_str().unwrap().to_owned();

    // GET and list.
    let (status, fetched, _) = h
        .json(
            axum::http::Method::GET,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched, route);
    let (status, list, _) = h
        .json(axum::http::Method::GET, "/oagw/v1/routes", &[], None)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["total"], json!(1));

    // A different `upstream_id` is rejected.
    let (status, problem, _) = h
        .json(
            axum::http::Method::PUT,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            Some(json!({
                "enabled": true,
                "upstream_id": "00000000-0000-0000-0000-000000000009",
                "match": {"http": {"methods": ["GET"], "path": "/v1/chat", "path_suffix_mode": "append"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.upstream_immutable.v1"
    );

    // Repeating the current value is accepted and the match is replaced.
    let (status, replaced, _) = h
        .json(
            axum::http::Method::PUT,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            Some(json!({
                "enabled": false,
                "upstream_id": upstream_id,
                "match": {"http": {"methods": ["PUT"], "path": "/v2/other", "path_suffix_mode": "append"}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], json!(false));
    assert_eq!(replaced["match"]["http"]["path"], "/v2/other");
    assert!(
        replaced["tags"]
            .as_array()
            .is_none_or(|tags| tags.is_empty()),
        "full replacement clears tags: {replaced}"
    );

    // An unknown upstream id is a validation failure, not a 404.
    let (status, problem) = create_route(
        &h,
        json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": {"http": {"methods": ["GET"], "path": "/x", "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["violations"][0]["field"], "upstream_id");

    // A duplicated match rule is a 409.
    let (status, second) = create_route(&h, json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["PUT"], "path": "/v2/other", "path_suffix_mode": "append"}}
    }))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{second}");
    assert_eq!(
        second["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.match_conflict.v1"
    );

    // A route match must carry at least one method and a leading-slash path.
    let (status, problem) = create_route(
        &h,
        json!({
            "upstream_id": upstream_id,
            "match": {"http": {"methods": [], "path": "/v1/x", "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["violations"][0]["field"], "match.http.methods");

    let (status, problem) = create_route(
        &h,
        json!({
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET"], "path": "v1/x", "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["violations"][0]["field"], "match.http.path");

    // DELETE returns 204, then the route is gone.
    let (status, body, _) = h
        .json(
            axum::http::Method::DELETE,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, problem, _) = h
        .json(
            axum::http::Method::GET,
            &format!("/oagw/v1/routes/{route_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn plugins_crud_source_and_delete_in_use() {
    let h = common::harness();
    let (status, plugin) = create_plugin(&h, inline_guard_plugin("correlation", "{}")).await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    assert_eq!(plugin["type"], "guard");
    assert_eq!(plugin["name"], "correlation");
    let plugin_id = plugin["id"].as_str().unwrap().to_owned();
    assert_eq!(
        plugin["gts_id"].as_str().unwrap(),
        format!("gts.cf.core.oagw.guard_plugin.v1~{plugin_id}"),
        "{}",
        plugin["gts_id"]
    );

    // A plugin without a name is rejected.
    let (status, problem) = create_plugin(&h, json!({"type": "guard", "name": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["violations"][0]["field"], "name");

    // The declared source is exposed verbatim.
    let (status, source, _) = h
        .json(
            axum::http::Method::GET,
            &format!("/oagw/v1/plugins/{plugin_id}/source"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert_eq!(source["kind"], "inline");
    assert_eq!(source["language"], "json");
    assert!(source["source_code"].as_str().is_some());

    // A plugin resource can be bound by UUID to an upstream chain.
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "alias": "bound",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]},
            "plugins": plugin_chain("private", &[&plugin_id])
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    assert_eq!(
        upstream["plugins"]["items"][0]["plugin_ref"]
            .as_str()
            .unwrap(),
        plugin_id,
        "{}",
        upstream["plugins"]
    );

    // Deleting a bound plugin is a 409 naming the referencing resources.
    let (status, problem, _) = h
        .json(
            axum::http::Method::DELETE,
            &format!("/oagw/v1/plugins/{plugin_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    assert_eq!(problem["status"], json!(409));
    assert_eq!(problem["plugin_id"].as_str().unwrap(), plugin["gts_id"]);
    assert_eq!(
        problem["referenced_by"]["upstreams"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // Once unbound the same delete succeeds.
    let (status, _) = put_upstream(
        &h,
        &upstream_id,
        json!({
            "alias": "bound",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = h
        .json(
            axum::http::Method::DELETE,
            &format!("/oagw/v1/plugins/{plugin_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, problem, _) = h
        .json(
            axum::http::Method::GET,
            &format!("/oagw/v1/plugins/{plugin_id}"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn cors_credentials_cannot_be_combined_with_a_wildcard() {
    let h = common::harness();
    let (status, problem) = create_upstream(
        &h,
        json!({
            "alias": "cors-bad",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]},
            "cors": {"enabled": true, "allow_credentials": true, "allowed_origins": ["*"],
                     "allowed_methods": ["GET"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        problem["violations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("wildcard origin"),
        "{}",
        problem
    );

    // A wildcard *method* is equally rejected for credentialed CORS.
    let (status, problem) = create_upstream(
        &h,
        json!({
            "alias": "cors-bad-2",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]},
            "cors": {"enabled": true, "allow_credentials": true,
                     "allowed_origins": ["https://app.example.com"], "allowed_methods": ["*"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["violations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("wildcard method"),
        "{}",
        problem
    );

    // A concrete origin with credentials is accepted.
    let (status, created) = create_upstream(
        &h,
        json!({
            "alias": "cors-ok",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]},
            "cors": {"enabled": true, "sharing": "inherit", "allow_credentials": true,
                     "allowed_origins": ["https://app.example.com"],
                     "allowed_methods": ["GET", "POST"],
                     "expose_headers": ["X-Request-Id"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["cors"]["sharing"], "inherit");
    assert_eq!(
        created["cors"]["allowed_origins"],
        json!(["https://app.example.com"])
    );
}

#[tokio::test]
async fn list_endpoints_page_with_top_and_skip() {
    let h = common::harness();
    for name in ["a-one", "b-two", "c-three"] {
        let (status, body) = create_upstream(&h, http_upstream("10.0.0.9", 8000, Some(name))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, page, _) = h
        .json(
            axum::http::Method::GET,
            "/oagw/v1/upstreams?$top=2&$skip=1",
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], json!(3));
    assert_eq!(page["items"].as_array().map(Vec::len), Some(2));
    // `$skip`/`$top` slice the tenant's upstreams; the storage is keyed by id,
    // so only membership is asserted, not ordering.
    let aliases: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["alias"].as_str())
        .collect();
    assert_eq!(aliases.len(), 2);
    assert!(
        aliases
            .iter()
            .all(|a| ["a-one", "b-two", "c-three"].contains(a))
    );
    assert!(!aliases.is_empty());

    // `$top` is clamped to the configured maximum (100).
    let (status, page, _) = h
        .json(
            axum::http::Method::GET,
            "/oagw/v1/upstreams?$top=9999",
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn upstream_rate_limit_configuration_round_trips() {
    let h = common::harness();
    let (status, created) = create_upstream(
        &h,
        json!({
            "alias": "limited",
            "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.9"}]},
            "rate_limit": {
                "sharing": "enforce",
                "algorithm": "sliding_window",
                "sustained": {"rate": 30, "window": "minute"},
                "burst": {"capacity": 10},
                "scope": "ip",
                "strategy": "degrade",
                "cost": 2
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let limit = &created["rate_limit"];
    assert_eq!(limit["sharing"], "enforce");
    assert_eq!(limit["algorithm"], "sliding_window");
    assert_eq!(limit["sustained"], json!({"rate": 30, "window": "minute"}));
    assert_eq!(limit["burst"]["capacity"], json!(10));
    assert_eq!(limit["scope"], "ip");
    assert_eq!(limit["strategy"], "degrade");
    assert_eq!(limit["cost"], json!(2));
}
