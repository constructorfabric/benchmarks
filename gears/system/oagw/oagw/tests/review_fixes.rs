//! Regression tests for the defects the code review found.
//!
//! Each test names the behaviour that was wrong before the fix, so a
//! reintroduction fails loudly rather than silently.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use axum::http::StatusCode;
use common::{FakeUpstream, Harness, header};

const SOURCE: &str = "x-oagw-error-source";

// ---- F1: a 429 must carry a wire Retry-After header --------------------

async fn wire_one_per_hour(h: &Harness, port: u16, alias: &str) {
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": alias,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": {"sustained": {"rate": 1, "window": "hour"}, "burst": {"capacity": 1}}
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;
}

#[tokio::test]
async fn a_rate_limit_rejection_carries_a_wire_retry_after_header() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    wire_one_per_hour(&h, up.port, "rl-header").await;

    let (s1, _, _) = h.raw("GET", "/oagw/v1/proxy/rl-header/x", None, &[]).await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/rl-header/x", None, &[]).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    // The header itself, not merely the JSON body's retry_after_seconds field.
    let retry = header(&hdrs, "retry-after").expect("429 must carry Retry-After");
    assert!(
        retry.parse::<u64>().is_ok(),
        "Retry-After must be a number of seconds, got {retry}"
    );
}

// ---- F6: chain ordering ------------------------------------------------

#[tokio::test]
async fn a_request_matching_no_route_is_404_even_when_cors_would_reject_it() {
    // CORS must be evaluated after the route is resolved, so an unmatched
    // request reports "no route" rather than being pre-empted by a CORS verdict.
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "ordered",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "cors": {"enabled": true, "allowed_origins": ["https://good.example"], "allowed_methods": ["GET"]}
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/allowed"}}
        })),
    )
    .await;

    let (s, hdrs, _) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/ordered/nowhere",
            None,
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "route resolution comes first");
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

// ---- F5/F3: target-host shape validation -------------------------------

#[tokio::test]
async fn a_target_host_carrying_a_scheme_or_path_is_a_distinct_invalid_error() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("shape", "http", up.port).await;

    for bad in [
        "http://127.0.0.1",
        "127.0.0.1/../x",
        "127.0.0.1:8080",
        "user@127.0.0.1",
    ] {
        let (s, _, body) = h
            .raw(
                "GET",
                "/oagw/v1/proxy/shape/x",
                None,
                &[("x-oagw-target-host", bad)],
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "`{bad}` should be rejected");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(
            text.contains("invalid target host"),
            "`{bad}` should be reported as invalid, not unknown: {text}"
        );
    }
}

#[tokio::test]
async fn a_well_formed_but_absent_target_host_is_reported_as_unknown() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("shape2", "http", up.port).await;

    let (s, _, body) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/shape2/x",
            None,
            &[("x-oagw-target-host", "other.example")],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(
        text.contains("unknown target host"),
        "a well-formed absent host is a distinct outcome: {text}"
    );
}

// ---- F7/F8: plugin identifiers other than a bare UUID ------------------

#[tokio::test]
async fn the_plugin_listing_merges_the_builtin_catalog_with_custom_definitions() {
    let h = Harness::graded();
    h.json(
        "POST",
        "/oagw/v1/plugins",
        Some(serde_json::json!({
            "name": "mine", "plugin_type": "guard", "source_code": "x"
        })),
    )
    .await;

    let (s, page) = h.json("GET", "/oagw/v1/plugins?$top=100", None).await;
    assert_eq!(s, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert!(
        items.iter().any(|i| i["origin"] == "builtin" && i["served"] == true),
        "a served built-in must appear"
    );
    assert!(
        items.iter().any(|i| i["origin"] == "builtin" && i["served"] == false),
        "a catalog-only entry must appear"
    );
    assert!(
        items.iter().any(|i| i["origin"] == "custom" && i["name"] == "mine"),
        "the tenant's own definition must appear"
    );
}

#[tokio::test]
async fn a_builtin_identifier_resolves_on_the_get_route() {
    let h = Harness::graded();
    let id = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    let (s, body) = h.json("GET", &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["served"], true);
    assert_eq!(body["plugin_type"], "guard");
}

#[tokio::test]
async fn an_unresolvable_named_identifier_is_404_not_a_routing_rejection() {
    let h = Harness::graded();
    let (s, _) = h.json("GET", "/oagw/v1/plugins/not-a-real-plugin", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_builtin_cannot_be_deleted() {
    let h = Harness::graded();
    let id = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    let (s, _) = h.json("DELETE", &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_builtin_has_no_source_text() {
    let h = Harness::graded();
    let id = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    let (s, _) = h
        .json("GET", &format!("/oagw/v1/plugins/{id}/source"), None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ---- F13: plugin definition validation ---------------------------------

#[tokio::test]
async fn a_plugin_definition_without_source_text_is_400() {
    let h = Harness::graded();
    let (s, _) = h
        .json(
            "POST",
            "/oagw/v1/plugins",
            Some(serde_json::json!({"name": "empty", "plugin_type": "guard"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_plugin_definition_with_a_non_object_config_schema_is_400() {
    let h = Harness::graded();
    let (s, _) = h
        .json(
            "POST",
            "/oagw/v1/plugins",
            Some(serde_json::json!({
                "name": "badschema", "plugin_type": "guard",
                "source_code": "x", "config_schema": "not-an-object"
            })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// ---- required-headers guard, end to end --------------------------------

async fn wire_guarded(h: &Harness, port: u16, alias: &str, request_headers: &str) {
    let (s, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": alias,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "plugins": {"items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]},
                "auth": {"config": {"required_request_headers": request_headers}}
            })),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "guarded upstream create failed: {created}");
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;
}

#[tokio::test]
async fn a_missing_required_request_header_is_400() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    wire_guarded(&h, up.port, "guarded", "x-needed").await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/guarded/x", None, &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));

    // Supplying it lets the request through.
    let (s, _, _) = h
        .raw("GET", "/oagw/v1/proxy/guarded/x", None, &[("x-needed", "1")])
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn an_all_blank_required_header_list_is_a_no_op() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    wire_guarded(&h, up.port, "blankguard", " , , ").await;
    let (s, _, _) = h.raw("GET", "/oagw/v1/proxy/blankguard/x", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
}

// ---- body cap ----------------------------------------------------------

#[tokio::test]
async fn a_declared_over_cap_request_body_is_413() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("cap", "http", up.port).await;

    let (s, hdrs, _) = h
        .raw(
            "POST",
            "/oagw/v1/proxy/cap/x",
            None,
            &[("content-length", "104857601")],
        )
        .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

// ---- WebTransport deferral --------------------------------------------

#[tokio::test]
async fn a_webtransport_upstream_is_501() {
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "wtsvc",
                "server": {"endpoints": [{"scheme": "wt", "host": "127.0.0.1", "port": 443}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/wtsvc/x", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

// ---- route management gaps the review flagged --------------------------

#[tokio::test]
async fn a_route_colliding_only_with_a_disabled_sibling_is_created() {
    let h = Harness::graded();
    let (_, up) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "dis",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 9}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = up["id"].as_str().unwrap().to_owned();

    let (s1, _) = h
        .json(
            "POST",
            "/oagw/v1/routes",
            Some(serde_json::json!({
                "upstream_id": id, "enabled": false,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}}
            })),
        )
        .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = h
        .json(
            "POST",
            "/oagw/v1/routes",
            Some(serde_json::json!({
                "upstream_id": id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}}
            })),
        )
        .await;
    assert_eq!(s2, StatusCode::CREATED, "a disabled sibling must not collide");
}

#[tokio::test]
async fn an_invalid_tag_is_400() {
    let h = Harness::graded();
    let (s, _) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "tagged", "tags": ["Not Valid"],
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 9}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_rate_limit_without_a_sustained_rate_is_400() {
    let h = Harness::graded();
    let (s, _) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "badrl",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 9}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": {"sustained": {"rate": 0}}
            })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// ---- proxy behaviours previously only unit-tested ----------------------

#[tokio::test]
async fn the_query_allowlist_filters_on_the_live_proxy_path() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "q",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/", "query_allowlist": ["keep"]}}
        })),
    )
    .await;

    let (s, _, body) = h
        .raw("GET", "/oagw/v1/proxy/q/x?keep=1&drop=2", None, &[])
        .await;
    assert_eq!(s, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let path = v["path"].as_str().unwrap();
    assert!(path.contains("keep=1"), "allowlisted param must survive: {path}");
    assert!(!path.contains("drop=2"), "other params must be dropped: {path}");
}

#[tokio::test]
async fn path_suffix_mode_disabled_drops_the_remainder_on_the_live_path() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "sfx",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/base", "path_suffix_mode": "disabled"}}
        })),
    )
    .await;

    let (s, _, body) = h.raw("GET", "/oagw/v1/proxy/sfx/base/extra", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["path"], "/base");
}
