//! Integration tests for the management `API`: upstreams, routes and plugins.
//!
//! Every request goes over a real socket to the router the gear registers, so
//! the status codes, problem documents and list parameters are exercised as the
//! host gateway would deliver them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    aliased_upstream_body, delete, get, post, put, route_body, upstream_body, uuid_of,
};
use oagw::domain::ids;

mod common;

const UPSTREAM_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";
const ROUTE_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// The `type` member of a problem document.
fn problem_type(response: &common::TestResponse) -> String {
    response.json()["type"].as_str().unwrap_or_default().to_owned()
}

/// The `detail` member of a problem document.
fn detail(response: &common::TestResponse) -> String {
    response.json()["detail"].as_str().unwrap_or_default().to_owned()
}

/// Creates an upstream through the `API` and returns the created body.
async fn create_upstream(
    harness: &std::sync::Arc<common::Harness>,
    body: Value,
) -> (u16, Value) {
    let response = post(harness.serve_once().await, "/oagw/v1/upstreams", body).await;
    (response.status, response.json())
}

/// Creates a route through the `API` and returns the created body.
async fn create_route(harness: &std::sync::Arc<common::Harness>, body: Value) -> (u16, Value) {
    let response = post(harness.serve_once().await, "/oagw/v1/routes", body).await;
    (response.status, response.json())
}

// ---------------------------------------------------------------------------------------
// Upstream CRUD
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_with_hostname_endpoint() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    assert_eq!(status, 201, "{created}");
    assert!(created["id"].as_str().unwrap_or_default().starts_with(UPSTREAM_PREFIX));
    let uuid = uuid_of(created["id"].as_str().unwrap_or_default());
    assert!(Uuid::parse_str(uuid).is_ok(), "the id must embed a uuid: {uuid}");
    assert_eq!(created["enabled"], json!(true));
    assert_eq!(created["alias"], json!("api.openai.com"));
    assert_eq!(
        created["server"]["endpoints"][0],
        json!({"scheme": "https", "host": "api.openai.com", "port": 443})
    );
    assert_eq!(
        created["protocol"],
        json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")
    );
}

#[tokio::test]
async fn read_back_a_created_upstream() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let fetched = get(addr, &format!("/oagw/v1/upstreams/{id}")).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.json()["id"], json!(id));
}

#[tokio::test]
async fn unknown_upstream_id_is_404() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let response = get(
        addr,
        &format!("/oagw/v1/upstreams/{UPSTREAM_PREFIX}{}", Uuid::nil()),
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(response.header("X-OAGW-Error-Source"), Some("gateway"));
    assert_eq!(problem_type(&response), ids::ERR_VALIDATION);
    assert_eq!(response.header("Content-Type"), Some("application/problem+json"));
    assert_eq!(response.json()["status"], json!(404));
    assert!(!detail(&response).is_empty());
}

#[tokio::test]
async fn delete_upstream_returns_204_and_the_resource_disappears() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(delete(addr, &format!("/oagw/v1/upstreams/{id}")).await.status, 204);
    assert_eq!(
        get(addr, &format!("/oagw/v1/upstreams/{id}")).await.status,
        404
    );
}

#[tokio::test]
async fn list_returns_created_upstreams() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    create_upstream(&harness, upstream_body("b.example.com", 443, "https")).await;
    let listing = get(addr, "/oagw/v1/upstreams").await;
    assert_eq!(listing.status, 200);
    assert_eq!(listing.json().as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn top_bounds_the_page() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    create_upstream(&harness, upstream_body("b.example.com", 443, "https")).await;
    let listing = get(addr, "/oagw/v1/upstreams?$top=1").await;
    assert_eq!(listing.json().as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn skip_and_orderby_apply() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    create_upstream(&harness, upstream_body("alpha.example.com", 443, "https")).await;
    create_upstream(&harness, upstream_body("beta.example.com", 443, "https")).await;
    let ordered = get(addr, "/oagw/v1/upstreams?$orderby=alias%20desc").await;
    let aliases: Vec<String> = ordered
        .json()
        .as_array()
        .expect("an array")
        .iter()
        .map(|row| row["alias"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(aliases, vec!["beta.example.com".to_owned(), "alpha.example.com".to_owned()]);

    let skipped = get(addr, "/oagw/v1/upstreams?$skip=1").await;
    assert_eq!(skipped.json().as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn filter_narrows_the_result() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    create_upstream(&harness, upstream_body("vendor.com", 443, "https")).await;
    create_upstream(&harness, upstream_body("other.example.com", 443, "https")).await;
    let listing = get(addr, "/oagw/v1/upstreams?$filter=alias%20eq%20%27vendor.com%27").await;
    let rows = listing.json().as_array().cloned().expect("an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["alias"], json!("vendor.com"));
}

#[tokio::test]
async fn select_projects_the_requested_fields() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    create_upstream(&harness, upstream_body("vendor.com", 443, "https")).await;
    let listing = get(addr, "/oagw/v1/upstreams?$select=alias").await;
    let rows = listing.json().as_array().cloned().expect("an array");
    let row = &rows[0];
    assert_eq!(row["alias"], json!("vendor.com"));
    assert!(row.get("id").is_none(), "the projection must drop other members");
}

// ---------------------------------------------------------------------------------------
// Alias derivation
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn single_hostname_with_standard_port_derives_the_hostname() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    assert_eq!(status, 201);
    assert_eq!(created["alias"], json!("api.openai.com"));
}

#[tokio::test]
async fn single_hostname_with_non_standard_port_derives_hostname_and_port() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("api.openai.com", 8443, "https")).await;
    assert_eq!(status, 201);
    assert_eq!(created["alias"], json!("api.openai.com:8443"));
}

#[tokio::test]
async fn multi_endpoint_common_suffix_derives_the_registrable_domain() {
    let harness = common::Harness::plain();
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "us.vendor.com", "port": 443},
            {"scheme": "https", "host": "eu.vendor.com", "port": 443}
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 201);
    assert_eq!(created["alias"], json!("vendor.com"));
}

#[tokio::test]
async fn bare_public_suffix_is_not_derivable() {
    let harness = common::Harness::plain();
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "foo.co.uk", "port": 443},
            {"scheme": "https", "host": "bar.co.uk", "port": 443}
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
    assert_eq!(problem_type_of(&created), ids::ERR_VALIDATION);
}

/// The `type` member of a problem document rendered as `JSON`.
fn problem_type_of(body: &Value) -> String {
    body["type"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn explicit_alias_is_required_for_ip_endpoints() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(&harness, upstream_body("10.0.1.1", 443, "https")).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn explicit_alias_is_accepted_for_ip_endpoints() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(
        &harness,
        aliased_upstream_body("10.0.1.1", 443, "https", "my-internal-service"),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["alias"], json!("my-internal-service"));
}

#[tokio::test]
async fn conflicting_user_provided_alias_for_a_derivable_endpoint_is_rejected() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(
        &harness,
        aliased_upstream_body("api.openai.com", 443, "https", "openai"),
    )
    .await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn exact_derived_alias_is_tolerated_for_idempotency() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(
        &harness,
        aliased_upstream_body("api.openai.com", 443, "https", "api.openai.com"),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["alias"], json!("api.openai.com"));
}

#[tokio::test]
async fn alias_is_normalised_to_lowercase() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("API.OpenAI.COM.", 443, "https")).await;
    assert_eq!(status, 201);
    assert_eq!(created["alias"], json!("api.openai.com"));
    assert_eq!(created["server"]["endpoints"][0]["host"], json!("api.openai.com"));
}

#[tokio::test]
async fn duplicate_alias_in_the_same_tenant_returns_409() {
    let harness = common::Harness::plain();
    let (first, _) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    assert_eq!(first, 201);
    let (second, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    assert_eq!(second, 409, "{created}");
    assert_eq!(problem_type_of(&created), ids::ERR_ALIAS_CONFLICT);
    assert_eq!(created["status"], json!(409));
    assert!(!detail_of(&created).is_empty());
}

#[tokio::test]
async fn replacing_endpoints_so_the_derived_alias_changes_is_rejected() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, created) = create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let replaced = put(
        addr,
        &format!("/oagw/v1/upstreams/{id}"),
        upstream_body("b.example.org", 443, "https"),
    )
    .await;
    assert_eq!(replaced.status, 400, "{}", replaced.text());
}

#[tokio::test]
async fn replacing_endpoints_that_keep_the_derived_alias_is_allowed() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, created) = create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let replaced = put(
        addr,
        &format!("/oagw/v1/upstreams/{id}"),
        upstream_body("a.example.com", 443, "https"),
    )
    .await;
    assert_eq!(replaced.status, 200, "{}", replaced.text());
    assert_eq!(replaced.json()["alias"], json!("a.example.com"));
}

#[tokio::test]
async fn a_replaced_upstream_keeps_its_identifier() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, created) = create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let mut body = upstream_body("a.example.com", 443, "https");
    body["tags"] = json!(["updated"]);
    let replaced = put(addr, &format!("/oagw/v1/upstreams/{id}"), body).await;
    assert_eq!(replaced.status, 200);
    assert_eq!(replaced.json()["id"], json!(id));
    assert_eq!(replaced.json()["tags"], json!(["updated"]));
}

// ---------------------------------------------------------------------------------------
// Endpoint scheme handling
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_http_upstream_is_accepted_when_plaintext_is_allowed() {
    let harness = common::Harness::allowing_http();
    let (status, created) = create_upstream(
        &harness,
        aliased_upstream_body("127.0.0.1", 8099, "http", "local-test"),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["server"]["endpoints"][0]["scheme"], json!("http"));
}

#[tokio::test]
async fn an_http_upstream_is_rejected_when_plaintext_is_not_allowed() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(
        &harness,
        aliased_upstream_body("127.0.0.1", 8099, "http", "local-test"),
    )
    .await;
    assert_eq!(status, 400, "{created}");
    assert!(detail_of(&created).contains("allow_http_upstream"));
}

/// The `detail` member of a problem document rendered as `JSON`.
fn detail_of(body: &Value) -> String {
    body["detail"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn an_unknown_scheme_is_rejected() {
    let harness = common::Harness::plain();
    let (status, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "ftp")).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn an_https_upstream_is_accepted_without_any_opt_in() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    assert_eq!(status, 201);
    assert_eq!(created["server"]["endpoints"][0]["scheme"], json!("https"));
}

// ---------------------------------------------------------------------------------------
// Endpoint pools and hostnames
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn mixed_scheme_endpoints_are_rejected() {
    let harness = common::Harness::plain();
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "http", "host": "a.example.com", "port": 80},
            {"scheme": "https", "host": "b.example.com", "port": 80}
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "mixed"
    });
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn mixed_port_endpoints_are_rejected() {
    let harness = common::Harness::plain();
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "a.example.com", "port": 80},
            {"scheme": "https", "host": "b.example.com", "port": 8080}
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "mixed-ports"
    });
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn an_empty_endpoint_list_is_rejected() {
    let harness = common::Harness::plain();
    let body = json!({
        "server": {"endpoints": []},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "empty"
    });
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn a_label_starting_with_a_hyphen_is_rejected() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("-bad.example.com", 443, "https")).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn a_label_over_sixty_three_characters_is_rejected() {
    let harness = common::Harness::plain();
    let long = "a".repeat(64);
    let (status, created) =
        create_upstream(&harness, upstream_body(&format!("{long}.example.com"), 443, "https")).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn a_trailing_dot_is_tolerated() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("api.openai.com.", 443, "https")).await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["server"]["endpoints"][0]["host"], json!("api.openai.com"));
}

#[tokio::test]
async fn a_zero_port_is_rejected() {
    let harness = common::Harness::plain();
    let (status, created) =
        create_upstream(&harness, upstream_body("api.openai.com", 0, "https")).await;
    assert_eq!(status, 400, "{created}");
}

// ---------------------------------------------------------------------------------------
// Upstream body validation
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_top_level_field_is_rejected() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["bogus"] = json!(1);
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn an_invalid_tag_is_rejected() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["tags"] = json!(["Not Allowed"]);
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
    assert!(detail_of(&created).contains("tag"));
}

#[tokio::test]
async fn a_rate_limit_without_sustained_rate_is_rejected() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["rate_limit"] = json!({"burst": {"capacity": 5}});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn allow_credentials_with_a_wildcard_origin_is_rejected() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["cors"] = json!({"enabled": true, "allowed_origins": ["*"], "allow_credentials": true});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn a_missing_protocol_is_rejected() {
    let harness = common::Harness::plain();
    let body = json!({"server": {"endpoints": [{"scheme": "https", "host": "a.example.com", "port": 443}]}});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

// ---------------------------------------------------------------------------------------
// Plugin bindings on an upstream
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_auth_plugin_type_is_rejected_at_create_time() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["auth"] = json!({"type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nosuch.v1"});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn an_unresolvable_plugin_reference_is_rejected_at_create_time() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["plugins"] =
        json!({"items": [{"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1"}]});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
    assert!(detail_of(&created).contains("cors"));
}

#[tokio::test]
async fn plugin_positions_must_start_at_zero_and_be_contiguous() {
    let harness = common::Harness::plain();
    let mut body = upstream_body("api.openai.com", 443, "https");
    body["plugins"] = json!({"items": [
        {"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1", "position": 0},
        {"plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1", "position": 2}
    ]});
    let (status, created) = create_upstream(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn catalog_only_guard_and_transform_identifiers_are_not_bindable() {
    let harness = common::Harness::plain();
    for reference in [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ] {
        let mut body = upstream_body("api.openai.com", 443, "https");
        body["plugins"] = json!({"items": [{"plugin_ref": reference}]});
        let (status, created) = create_upstream(&harness, body).await;
        assert_eq!(status, 400, "{reference} -> {created}");
    }
}

#[tokio::test]
async fn catalog_only_auth_identifiers_do_not_resolve() {
    let harness = common::Harness::plain();
    for family in ["basic", "bearer"] {
        let mut body = upstream_body("api.openai.com", 443, "https");
        body["auth"] =
            json!({"type": format!("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.{family}.v1")});
        let (status, created) = create_upstream(&harness, body).await;
        assert_eq!(status, 400, "{family} -> {created}");
        assert!(detail_of(&created).contains("unknown auth plugin"));
    }
}

#[tokio::test]
async fn both_oauth2_variants_are_accepted_at_create_time() {
    let harness = common::Harness::plain();
    for (index, family) in ["oauth2_client_cred", "oauth2_client_cred_basic"]
        .iter()
        .enumerate()
    {
        let mut body = upstream_body(&format!("host{index}.example.com"), 443, "https");
        body["auth"] = json!({
            "type": format!("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.{family}.v1"),
            "config": {"token_endpoint": "https://auth.example.com/token"}
        });
        let (status, created) = create_upstream(&harness, body).await;
        assert_eq!(status, 201, "{family} -> {created}");
    }
}

// ---------------------------------------------------------------------------------------
// Tenant scoping
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn another_tenants_upstream_is_not_visible() {
    let harness = common::Harness::plain();
    let (_, created) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();

    let other = harness.with_tenant(Uuid::new_v4());
    let response = get(other.serve_once().await, &format!("/oagw/v1/upstreams/{id}")).await;
    drop(other);
    assert_eq!(response.status, 404);
}

#[tokio::test]
async fn the_same_alias_may_exist_in_two_tenants() {
    let harness = common::Harness::plain();
    let first = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;

    let other = harness.with_tenant(Uuid::new_v4());
    let second = create_upstream(&other, upstream_body("api.openai.com", 443, "https")).await;
    drop(other);
    assert_eq!(first.0, 201);
    assert_eq!(second.0, 201);
    assert_ne!(first.1["id"], second.1["id"]);
}

#[tokio::test]
async fn list_is_scoped_to_the_calling_tenant() {
    let harness = common::Harness::plain();
    create_upstream(&harness, upstream_body("mine.example.com", 443, "https")).await;

    let other = harness.with_tenant(Uuid::new_v4());
    let listing = get(other.serve_once().await, "/oagw/v1/upstreams").await;
    drop(other);
    assert_eq!(listing.json().as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------------------
// Route CRUD
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn create_and_read_back_a_route() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {
            "methods": ["GET", "POST"],
            "path": "/v1",
            "path_suffix_mode": "append",
            "query_allowlist": []
        }}
    });
    let (status, created) = create_route(&harness, body.clone()).await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    assert!(id.starts_with(ROUTE_PREFIX));
    assert!(Uuid::parse_str(uuid_of(&id)).is_ok());

    let fetched = get(addr, &format!("/oagw/v1/routes/{id}")).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.json()["id"], json!(id));
    assert_eq!(fetched.json()["match"]["http"]["methods"], json!(["GET", "POST"]));
    assert_eq!(fetched.json()["enabled"], json!(true));
}

#[tokio::test]
async fn replace_a_route() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let (_, created) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v1")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();

    let replaced = put(addr, &format!("/oagw/v1/routes/{id}"), route_body(&upstream_id, &["PUT"], "/v1")).await;
    assert_eq!(replaced.status, 200, "{}", replaced.text());
    assert_eq!(replaced.json()["match"]["http"]["methods"], json!(["PUT"]));

    let fetched = get(addr, &format!("/oagw/v1/routes/{id}")).await;
    assert_eq!(fetched.json()["match"]["http"]["methods"], json!(["PUT"]));
}

#[tokio::test]
async fn delete_a_route() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let (_, created) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v1")).await;
    let id = created["id"].as_str().unwrap_or_default().to_owned();

    assert_eq!(delete(addr, &format!("/oagw/v1/routes/{id}")).await.status, 204);
    assert_eq!(get(addr, &format!("/oagw/v1/routes/{id}")).await.status, 404);
}

#[tokio::test]
async fn an_unknown_route_id_is_404() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let response = get(addr, &format!("/oagw/v1/routes/{ROUTE_PREFIX}{}", Uuid::nil())).await;
    assert_eq!(response.status, 404);
}

#[tokio::test]
async fn route_to_a_nonexistent_upstream_is_rejected() {
    let harness = common::Harness::plain();
    let body = route_body(&format!("{UPSTREAM_PREFIX}{}", Uuid::nil()), &["GET"], "/v1");
    let (status, created) = create_route(&harness, body).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn route_to_another_tenants_upstream_is_rejected() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();

    let other = harness.with_tenant(Uuid::new_v4());
    let (status, created) = create_route(&other, route_body(&upstream_id, &["GET"], "/v1")).await;
    drop(other);
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn route_upstream_id_is_immutable() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, first) = create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    let (_, second) = create_upstream(&harness, upstream_body("b.example.com", 443, "https")).await;
    let first_id = first["id"].as_str().unwrap_or_default().to_owned();
    let second_id = second["id"].as_str().unwrap_or_default().to_owned();
    let (_, route) = create_route(&harness, route_body(&first_id, &["GET"], "/v1")).await;
    let route_id = route["id"].as_str().unwrap_or_default().to_owned();

    let replaced = put(addr, &format!("/oagw/v1/routes/{route_id}"), route_body(&second_id, &["GET"], "/v1")).await;
    assert_eq!(replaced.status, 400, "{}", replaced.text());
}

#[tokio::test]
async fn route_match_rules_are_validated() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();

    let cases = vec![
        json!({"upstream_id": upstream_id}),
        json!({"upstream_id": upstream_id, "match": {"http": {"methods": [], "path": "/x"}}}),
        json!({"upstream_id": upstream_id, "match": {"http": {"methods": ["TRACE"], "path": "/x"}}}),
        json!({"upstream_id": upstream_id, "match": {"http": {"methods": ["GET"], "path": ""}}}),
        json!({"upstream_id": upstream_id, "match": {"http": {"methods": ["GET"], "path": "v1"}}}),
        json!({"upstream_id": upstream_id, "match": {}}),
        json!({
            "upstream_id": upstream_id,
            "match": {
                "http": {"methods": ["GET"], "path": "/x"},
                "grpc": {"service": "svc", "method": "m"}
            }
        }),
    ];
    for body in cases {
        let (status, created) = create_route(&harness, body).await;
        assert_eq!(status, 400, "{created}");
    }
}

#[tokio::test]
async fn a_grpc_match_is_accepted_as_a_catalogued_match_form() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}}
    });
    let (status, created) = create_route(&harness, body).await;
    assert_eq!(status, 201, "{created}");
}

#[tokio::test]
async fn duplicate_path_and_method_returns_409() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let (first, _) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v1")).await;
    assert_eq!(first, 201);
    let (second, created) = create_route(&harness, route_body(&upstream_id, &["GET", "PATCH"], "/v1")).await;
    assert_eq!(second, 409, "{created}");
    assert_eq!(problem_type_of(&created), ids::ERR_ROUTE_CONFLICT);
}

#[tokio::test]
async fn distinct_paths_do_not_conflict() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let (first, _) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v1")).await;
    let (second, _) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v2")).await;
    assert_eq!(first, 201);
    assert_eq!(second, 201);
}

#[tokio::test]
async fn a_disabled_route_can_be_replaced_by_one_with_the_same_match() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let (_, route) = create_route(&harness, route_body(&upstream_id, &["GET"], "/v1")).await;
    let route_id = route["id"].as_str().unwrap_or_default().to_owned();

    let mut body = route_body(&upstream_id, &["GET"], "/v1");
    body["enabled"] = json!(false);
    assert_eq!(
        put(addr, &format!("/oagw/v1/routes/{route_id}"), body).await.status,
        200
    );
}

#[tokio::test]
async fn route_bodies_are_validated() {
    let harness = common::Harness::plain();
    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();

    let mut unknown = route_body(&upstream_id, &["GET"], "/v1");
    unknown["bogus"] = json!(1);
    let (status, created) = create_route(&harness, unknown).await;
    assert_eq!(status, 400, "{created}");

    let mut tags = route_body(&upstream_id, &["GET"], "/v1");
    tags["tags"] = json!(["Not Allowed"]);
    let (status, created) = create_route(&harness, tags).await;
    assert_eq!(status, 400, "{created}");

    let mut limit = route_body(&upstream_id, &["GET"], "/v1");
    limit["rate_limit"] = json!({"strategy": "reject"});
    let (status, created) = create_route(&harness, limit).await;
    assert_eq!(status, 400, "{created}");

    let (status, created) = create_route(&harness, json!({"upstream_id": upstream_id, "match": {"http": {"methods": ["GET"], "path": "/v1"}}, "tags": ["bad tag"] })).await;
    assert_eq!(status, 400, "{created}");
}

#[tokio::test]
async fn filter_routes_by_upstream() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let (_, first) = create_upstream(&harness, upstream_body("a.example.com", 443, "https")).await;
    let (_, second) = create_upstream(&harness, upstream_body("b.example.com", 443, "https")).await;
    let first_id = first["id"].as_str().unwrap_or_default().to_owned();
    let second_id = second["id"].as_str().unwrap_or_default().to_owned();
    create_route(&harness, route_body(&first_id, &["GET"], "/one")).await;
    create_route(&harness, route_body(&second_id, &["GET"], "/two")).await;

    let listing = get(addr, &format!("/oagw/v1/routes?$filter=upstream_id%20eq%20%27{first_id}%27")).await;
    let rows = listing.json().as_array().cloned().expect("an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["upstream_id"], json!(first_id));
}

// ---------------------------------------------------------------------------------------
// Plugin management
// ---------------------------------------------------------------------------------------

fn plugin_body() -> Value {
    json!({
        "name": "my-guard",
        "plugin_type": "guard",
        "source_code": "def on_request(ctx):\n    return ctx.next()\n"
    })
}

#[tokio::test]
async fn create_and_read_a_plugin() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let response = post(addr, "/oagw/v1/plugins", plugin_body()).await;
    assert_eq!(response.status, 201, "{}", response.text());
    let created = response.json();
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    assert!(id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"), "{created}");

    let fetched = get(addr, &format!("/oagw/v1/plugins/{id}")).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.json()["id"], json!(id));
    assert_eq!(fetched.json()["name"], json!("my-guard"));
}

#[tokio::test]
async fn plugin_source_is_retrievable() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let created = post(addr, "/oagw/v1/plugins", plugin_body()).await.json();
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let source = get(addr, &format!("/oagw/v1/plugins/{id}/source")).await;
    assert_eq!(source.status, 200);
    assert_eq!(source.text(), "def on_request(ctx):\n    return ctx.next()\n");
}

#[tokio::test]
async fn plugins_are_immutable() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let created = post(addr, "/oagw/v1/plugins", plugin_body()).await.json();
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    let response = put(addr, &format!("/oagw/v1/plugins/{id}"), plugin_body()).await;
    assert_eq!(response.status, 405, "{}", response.text());
    assert_eq!(
        response.header("X-OAGW-Error-Source"),
        Some("gateway")
    );
}

#[tokio::test]
async fn deleting_an_in_use_plugin_returns_409() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let created = post(addr, "/oagw/v1/plugins", plugin_body()).await.json();
    let id = created["id"].as_str().unwrap_or_default().to_owned();

    let (_, upstream) = create_upstream(&harness, upstream_body("api.openai.com", 443, "https")).await;
    let upstream_id = upstream["id"].as_str().unwrap_or_default().to_owned();
    let mut body = route_body(&upstream_id, &["GET"], "/v1");
    body["plugins"] = json!({"items": [{"plugin_ref": id}]});
    let (status, created) = create_route(&harness, body).await;
    assert_eq!(status, 201, "{created}");
    let route_id = created["id"].as_str().unwrap_or_default().to_owned();

    let response = delete(addr, &format!("/oagw/v1/plugins/{id}")).await;
    assert_eq!(response.status, 409, "{}", response.text());
    assert_eq!(problem_type(&response), ids::ERR_PLUGIN_IN_USE);
    let referenced_by = &response.json()["referenced_by"];
    assert!(referenced_by["routes"].is_array(), "{referenced_by}");
    assert_eq!(referenced_by["routes"][0], json!(route_id));
    assert!(detail(&response).contains("referenced by"));
}

#[tokio::test]
async fn deleting_an_unused_plugin_returns_204() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let created = post(addr, "/oagw/v1/plugins", plugin_body()).await.json();
    let id = created["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(delete(addr, &format!("/oagw/v1/plugins/{id}")).await.status, 204);
    assert_eq!(get(addr, &format!("/oagw/v1/plugins/{id}")).await.status, 404);
}

#[tokio::test]
async fn plugin_lists_and_types() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    post(addr, "/oagw/v1/plugins", plugin_body()).await;
    let mut body = plugin_body();
    body["name"] = json!("my-transform");
    body["plugin_type"] = json!("transform");
    post(addr, "/oagw/v1/plugins", body).await;

    let listing = get(addr, "/oagw/v1/plugins").await;
    assert_eq!(listing.json().as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn an_empty_plugin_name_is_rejected() {
    let harness = common::Harness::plain();
    let addr = harness.serve_once().await;
    let mut body = plugin_body();
    body["name"] = json!("   ");
    let response = post(addr, "/oagw/v1/plugins", body).await;
    assert_eq!(response.status, 400, "{}", response.text());
}
