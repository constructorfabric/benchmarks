//! Integration tests of the OAGW management API (slice 2).
//!
//! Every test drives the real handler stack — extractors, the
//! `ControlPlaneService`, the registry and the ADR-0007 error-source layer —
//! through `tower::ServiceExt::oneshot`, using the router builder in
//! `crate::api::rest::test_support` instead of a `GearCtx`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use oagw::api::rest::test_support::{
    TestApp, body, build_app, build_app_without_hierarchy, caller, request,
};
use oagw::config::OagwConfig;
use oagw::domain::error::OagwError;
use oagw::domain::model::SharingMode;
use oagw::domain::services::TenantHierarchy;
use serde_json::{Value, json};
use uuid::Uuid;

use async_trait::async_trait;
use toolkit_security::SecurityContext;

const TENANT: Uuid = Uuid::from_u128(0x11);
const OTHER: Uuid = Uuid::from_u128(0x22);

/// Body of a minimal valid upstream draft.
fn upstream_body(alias: Option<&str>, hosts: &[&str]) -> Value {
    json!({
        "server": {"endpoints": hosts.iter().map(|host| {
            json!({"scheme": "https", "host": host, "port": 443})
        }).collect::<Vec<_>>()},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": alias,
    })
}

/// A single-host upstream draft, so the alias is derived from the hostname.
fn derived_upstream(host: &str) -> Value {
    upstream_body(None, &[host])
}

/// Body of a minimal valid route draft.
fn route_body(path: &str, methods: &[&str]) -> Value {
    json!({
        "match": {"http": {"methods": methods, "path": path, "path_suffix_mode": "append"}},
        "headers": {},
        "plugins": {},
    })
}

/// Adds an `x-request-id` header, returning the request for `call`.
fn with_request_id(
    req: Result<axum::http::Request<axum::body::Body>, axum::http::Error>,
    id: &str,
) -> Result<axum::http::Request<axum::body::Body>, axum::http::Error> {
    let mut built = req.expect("request builds");
    built.headers_mut().insert(
        "x-request-id",
        axum::http::HeaderValue::from_str(id).expect("valid header value"),
    );
    Ok(built)
}

/// Sends a request and returns `(status, body)`.
async fn call(
    app: &mut TestApp,
    req: Result<axum::http::Request<axum::body::Body>, axum::http::Error>,
) -> (axum::http::StatusCode, Value) {
    let response = app
        .send(req.expect("request builds"))
        .await
        .expect("infallible");
    let status = response.status();
    let text = body(response).await.expect("body reads");
    let parsed = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).expect("body is JSON")
    };
    (status, parsed)
}

/// Hierarchy that pins a single ancestor, for the bind-rule test.
struct FixedHierarchy;

#[async_trait]
impl TenantHierarchy for FixedHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, _tenant: Uuid) -> Vec<Uuid> {
        vec![OTHER]
    }
}

async fn app() -> TestApp {
    build_app_without_hierarchy(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    })
}

fn ctx(tenant: Uuid) -> toolkit_security::SecurityContext {
    caller(tenant).expect("security context")
}

// -- upstream lifecycle ---------------------------------------------------------

#[tokio::test]
async fn create_upstream_derives_the_alias_and_returns_201() {
    let mut app = app().await;
    let (status, created) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    assert_eq!(created["alias"], json!("api.openai.com"));
    assert_eq!(created["enabled"], json!(true), "enabled defaults to true");
    assert_eq!(
        created["server"]["endpoints"][0]["host"],
        json!("api.openai.com")
    );
    assert_eq!(
        created["protocol"],
        json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")
    );
    assert!(
        created["created_at"].is_string(),
        "timestamps are on the wire"
    );
    assert!(
        created["tenant_id"].is_null(),
        "tenant_id stays off the wire"
    );

    // The id is a bare UUID and the Location header names the same resource.
    let id = created["id"].as_str().expect("id is a string").to_owned();
    assert!(Uuid::parse_str(&id).is_ok(), "{id}");
    let (status, read) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "the id round-trips: {read}"
    );
}

#[tokio::test]
async fn the_created_response_carries_a_location_header() {
    let mut app = app().await;
    let response = app
        .send(
            request(
                "POST",
                "/oagw/v1/upstreams",
                ctx(TENANT),
                Some(&derived_upstream("vendor.com").to_string()),
            )
            .expect("request builds"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let id = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .map(ToOwned::to_owned)
        .expect("Location names the new resource");
    let (status, read) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{read}");
}

#[tokio::test]
async fn a_duplicate_alias_is_a_409_and_a_foreign_alias_is_invisible() {
    let mut app = app().await;
    let first = json!({"alias": "payments", "server": {"endpoints": [
        {"scheme": "https", "host": "10.0.0.1", "port": 443}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"});
    let (status, _) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&first.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&first.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.conflict.v1"),
        "{body}"
    );

    // The same alias in another tenant is a different resource.
    let (status, _) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(OTHER),
            Some(&first.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
}

/// `unmatched` is the literal the data-plane metrics fold unresolved requests
/// onto, so an upstream may not claim it — through a supplied alias or through
/// the hostname a pool derives it from.
#[tokio::test]
async fn the_reserved_alias_unmatched_is_a_400() {
    let mut app = app().await;
    for body in [
        json!({"alias": "unmatched", "server": {"endpoints": [
            {"scheme": "https", "host": "10.0.0.1", "port": 443}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"}),
        derived_upstream("unmatched"),
    ] {
        let (status, response) = call(
            &mut app,
            request(
                "POST",
                "/oagw/v1/upstreams",
                ctx(TENANT),
                Some(&body.to_string()),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{response}");
        assert!(
            response["detail"]
                .as_str()
                .expect("detail")
                .contains("reserved"),
            "{response}"
        );
    }
}

#[tokio::test]
async fn an_invalid_upstream_is_a_400_with_the_error_source_header() {
    let mut app = app().await;
    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api:8443:x").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"].as_str().expect("detail").contains("host"),
        "{body}"
    );

    // A server-generated field is rejected before the handler runs.
    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(
                r#"{"server": {"endpoints": []}, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "tenant_id": "abc"}"#,
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("unknown field"),
        "{body}"
    );
}

#[tokio::test]
async fn a_malformed_body_is_a_400_with_the_gateway_error_source() {
    let mut app = app().await;
    let response = app
        .send(
            request("POST", "/oagw/v1/upstreams", ctx(TENANT), Some("{not json"))
                .expect("request builds"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .map(|value| value.to_str().expect("value")),
        Some("gateway"),
        "ADR-0007 marks management-API errors as gateway-originated"
    );
}

#[tokio::test]
async fn a_malformed_path_id_is_a_problem_document_not_a_plain_text_400() {
    let mut app = app().await;
    for (path, detail, invalid) in [
        (
            "/oagw/v1/upstreams/not-a-uuid",
            "invalid upstream id",
            "not-a-uuid",
        ),
        (
            "/oagw/v1/routes/not-a-uuid",
            "invalid route id",
            "not-a-uuid",
        ),
        (
            "/oagw/v1/plugins/not-a-uuid",
            "invalid plugin id",
            "not-a-uuid",
        ),
        // A GTS-form id of the *wrong* resource type is just as malformed.
        (
            "/oagw/v1/upstreams/gts.cf.core.oagw.plugin.v1~00000000-0000-0000-0000-000000000001",
            "invalid upstream id",
            "gts.cf.core.oagw.plugin.v1~00000000-0000-0000-0000-000000000001",
        ),
    ] {
        let response = app
            .send(request("GET", path, ctx(TENANT), None).expect("request builds"))
            .await
            .expect("infallible");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{path}"
        );
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .map(|value| value.to_str().expect("value")),
            Some("application/problem+json"),
            "{path}"
        );
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .map(|value| value.to_str().expect("value")),
            Some("gateway"),
            "{path}"
        );
        let text = body(response).await.expect("body reads");
        let problem: Value = serde_json::from_str(&text).expect("problem json, not plain text");
        assert_eq!(problem["status"], json!(404), "{path}: {problem}");
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.not_found.v1"),
            "{path}: {problem}"
        );
        assert_eq!(problem["detail"], json!(detail), "{path}: {problem}");
        assert_eq!(
            problem["context"]["invalid_value"],
            json!(invalid),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_well_formed_path_id_still_reads_a_missing_resource_as_a_404() {
    let mut app = app().await;
    let (status, problem) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-00000000abcd",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["detail"],
        json!(
            "upstream gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-00000000abcd does not exist"
        ),
        "{problem}"
    );
    // The GTS-form spelling of the same id reads the same resource.
    let (status, problem) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/upstreams/gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-00000000abcd",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["status"], json!(404), "{problem}");
}

#[tokio::test]
async fn upstream_read_replace_and_delete_round_trip() {
    let mut app = app().await;
    let (status, created) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();
    let path = format!("/oagw/v1/upstreams/{id}");

    // GET
    let (status, read) = call(&mut app, request("GET", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{read}");
    assert_eq!(read["alias"], json!("api.openai.com"));

    // GET from another tenant is a 404, not a 403.
    let (status, missing) = call(&mut app, request("GET", &path, ctx(OTHER), None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{missing}");
    assert!(
        missing["detail"]
            .as_str()
            .expect("detail")
            .contains("upstream"),
        "{missing}"
    );

    // PUT keeps the derived alias and clears omitted sections.
    let replacement = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "eu.api.openai.com", "port": 443},
            {"scheme": "https", "host": "us.api.openai.com", "port": 443}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "tags": ["team-a"],
        "enabled": false,
    });
    let (status, replaced) = call(
        &mut app,
        with_request_id(
            request("PUT", &path, ctx(TENANT), Some(&replacement.to_string())),
            "req-9",
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{replaced}");
    assert_eq!(
        replaced["alias"],
        json!("api.openai.com"),
        "the alias is immutable"
    );
    assert_eq!(replaced["enabled"], json!(false));
    assert_eq!(replaced["tags"], json!(["team-a"]));
    assert_eq!(
        replaced["server"]["endpoints"].as_array().map(Vec::len),
        Some(2)
    );

    // A PUT whose pool would derive a different alias is a 400.
    let renamed = upstream_body(None, &["api.vendor.com"]);
    let (status, body) = call(
        &mut app,
        request("PUT", &path, ctx(TENANT), Some(&renamed.to_string())),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("immutable"),
        "{body}"
    );
    assert_eq!(body["context"]["alias"], json!("api.openai.com"), "{body}");

    // DELETE then GET is a 404.
    let (status, _) = call(
        &mut app,
        with_request_id(request("DELETE", &path, ctx(TENANT), None), "req-10"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = call(&mut app, request("GET", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let mut app = app().await;
    let (_, upstream) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let (_, route) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            ctx(TENANT),
            Some(&route_for(&upstream_id, "/v1", &["GET"]).to_string()),
        ),
    )
    .await;
    assert_eq!(route["upstream_id"], json!(upstream_id), "{route}");

    let (status, _) = call(
        &mut app,
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/routes/{}", route["id"].as_str().expect("id")),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "the route cascade is observable"
    );
}

/// A route draft bound to `upstream_id`.
fn route_for(upstream_id: &str, path: &str, methods: &[&str]) -> Value {
    let mut draft = route_body(path, methods);
    draft["upstream_id"] = json!(upstream_id);
    draft
}

#[tokio::test]
async fn a_route_for_a_foreign_upstream_is_a_404() {
    let mut app = app().await;
    let (_, foreign) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(OTHER),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let foreign_id = foreign["id"].as_str().expect("id").to_owned();
    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            ctx(TENANT),
            Some(&route_for(&foreign_id, "/v1", &["GET"]).to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");
}

// -- route lifecycle ------------------------------------------------------------

#[tokio::test]
async fn route_crud_and_duplicate_match_rules() {
    let mut app = app().await;
    let (_, upstream) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (status, created) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            ctx(TENANT),
            Some(&route_for(&upstream_id, "/v1", &["GET", "POST"]).to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    assert_eq!(created["match"]["http"]["path"], json!("/v1"));
    assert_eq!(created["match"]["http"]["methods"], json!(["GET", "POST"]));
    assert_eq!(created["priority"], json!(0));
    assert_eq!(created["upstream_id"], json!(upstream_id));
    let route_id = created["id"].as_str().expect("id").to_owned();

    // The same match rule at the same priority is a 409.
    let (status, conflict) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            ctx(TENANT),
            Some(&route_for(&upstream_id, "/v1", &["POST", "GET"]).to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.conflict.v1")
    );

    // A route whose match protocol differs from the upstream is a 400.
    let grpc = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "svc.v1.Svc", "method": "Get"}},
        "headers": {},
        "plugins": {},
    });
    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            ctx(TENANT),
            Some(&grpc.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");

    // A replace carries no upstream_id; sending one is a 400.
    let (status, body) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            ctx(TENANT),
            Some(&route_for(&upstream_id, "/v2", &["GET"]).to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("upstream_id"),
        "{body}"
    );

    let mut replacement = route_body("/v2", &["GET"]);
    replacement["priority"] = json!(5);
    let (status, replaced) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            ctx(TENANT),
            Some(&replacement.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{replaced}");
    assert_eq!(replaced["match"]["http"]["path"], json!("/v2"));
    assert_eq!(replaced["priority"], json!(5));
    assert_eq!(
        replaced["upstream_id"],
        json!(upstream_id),
        "upstream_id is immutable"
    );

    let (status, _) = call(
        &mut app,
        request(
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/routes/{route_id}"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

// -- list queries ---------------------------------------------------------------

async fn seed_routes(app: &mut TestApp) -> String {
    let (_, upstream) = call(
        app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    for (path, priority) in [("/v1", 1), ("/v2", 3), ("/v3", 2)] {
        let mut draft = route_for(&upstream_id, path, &["GET"]);
        draft["priority"] = json!(priority);
        let (status, body) = call(
            app,
            request(
                "POST",
                "/oagw/v1/routes",
                ctx(TENANT),
                Some(&draft.to_string()),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    }
    upstream_id
}

#[tokio::test]
async fn the_route_list_supports_filter_order_select_top_and_skip() {
    let mut app = app().await;
    let _unused = seed_routes(&mut app).await;

    // $orderby + $skip + $top
    let (status, page) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/routes?$orderby=priority%20desc&$top=2&$skip=1",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{page}");
    assert_eq!(page["page_info"]["limit"], json!(2));
    assert!(page["page_info"]["next_cursor"].is_null(), "{page}");
    let priorities: Vec<i64> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["priority"].as_i64().expect("priority"))
        .collect();
    assert_eq!(priorities, vec![2, 1], "{page}");

    // $filter + $select
    let (status, page) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/routes?$filter=priority%20eq%202&$select=priority,match",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
    let only = &page["items"][0];
    assert_eq!(only["priority"], json!(2));
    assert_eq!(only["match"]["http"]["path"], json!("/v3"));
    assert!(
        only.get("id").is_none(),
        "$select drops the other fields: {only}"
    );

    // An unknown field is a 400, not a silent unfiltered list.
    let (status, body) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/routes?$filter=tenant_id%20eq%20%27x%27",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("tenant_id"),
        "{body}"
    );

    // An unknown $ parameter is a 400.
    let (status, body) = call(
        &mut app,
        request(
            "GET",
            "/oagw/v1/routes?$filtert=priority%20eq%201",
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn the_upstream_list_projects_selects_and_keeps_its_own_tenant() {
    let mut app = app().await;
    for host in ["api.openai.com", "vendor.com"] {
        let (status, _) = call(
            &mut app,
            request(
                "POST",
                "/oagw/v1/upstreams",
                ctx(TENANT),
                Some(&derived_upstream(host).to_string()),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
    }
    let (status, _) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(OTHER),
            Some(&derived_upstream("other.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let (status, page) = call(
        &mut app,
        request("GET", "/oagw/v1/upstreams?$select=alias", ctx(TENANT), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{page}");
    let aliases: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["alias"].as_str().expect("alias"))
        .collect();
    assert!(aliases.contains(&"api.openai.com"), "{aliases:?}");
    assert!(
        !aliases.contains(&"other.com"),
        "lists are tenant-scoped: {aliases:?}"
    );
}

// -- plugins --------------------------------------------------------------------

async fn seed_plugin(app: &mut TestApp) -> String {
    let draft = json!({
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1",
        "config": {"headers": ["x-request-id"]},
    });
    let (status, created) = call(
        app,
        request(
            "POST",
            "/oagw/v1/plugins",
            ctx(TENANT),
            Some(&draft.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    created["id"].as_str().expect("id").to_owned()
}

#[tokio::test]
async fn plugin_crud_and_the_in_use_conflict_body() {
    let mut app = app().await;
    let plugin_id = seed_plugin(&mut app).await;
    let path = format!("/oagw/v1/plugins/{plugin_id}");

    let (status, read) = call(&mut app, request("GET", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{read}");
    assert_eq!(
        read["plugin_type"],
        json!("gts.cf.core.oagw.guard_plugin.v1")
    );
    assert_eq!(read["config"]["headers"], json!(["x-request-id"]));

    // The rendered source is stable and names the plugin.
    let (status, source) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/plugins/{plugin_id}/source"),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{source}");
    assert_eq!(source["plugin_id"], json!(plugin_id));
    assert!(
        source["source"]
            .as_str()
            .expect("source")
            .contains("PLUGIN_TYPE"),
        "{source}"
    );

    // Bind the plugin into an upstream chain, then attempt a delete.
    let (_, upstream) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let mut draft = derived_upstream("api.openai.com");
    draft["plugins"] = json!({"items": [plugin_id]});
    let (status, body) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            ctx(TENANT),
            Some(&draft.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["plugins"]["items"][0], json!(plugin_id), "{body}");

    let (status, conflict) = call(&mut app, request("DELETE", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"),
        "{conflict}"
    );
    assert_eq!(
        conflict["context"]["plugin_id"],
        json!(format!("gts.cf.core.oagw.plugin.v1~{plugin_id}"))
    );
    let referenced = &conflict["context"]["referenced_by"];
    assert_eq!(
        referenced["upstreams"],
        json!([format!("gts.cf.core.oagw.upstream.v1~{upstream_id}")]),
        "{conflict}"
    );

    // Unbind, then the delete succeeds.
    let (status, _) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, _) = call(&mut app, request("DELETE", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = call(&mut app, request("GET", &path, ctx(TENANT), None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_invalid_plugin_type_is_a_400_and_an_unknown_plugin_is_a_404() {
    let mut app = app().await;
    let draft = json!({"plugin_type": "not-a-gts-id"});
    let (status, body) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/plugins",
            ctx(TENANT),
            Some(&draft.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(
        &mut app,
        request(
            "GET",
            &format!("/oagw/v1/plugins/{}", Uuid::from_u128(0x999)),
            ctx(TENANT),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["detail"].as_str().expect("detail").contains("plugin"),
        "{body}"
    );
}

// -- hierarchy ------------------------------------------------------------------

#[tokio::test]
async fn an_enforced_ancestor_alias_is_a_403() {
    let mut app = build_app(
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        Arc::new(FixedHierarchy),
    );
    let (_, ancestor) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(OTHER),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    let ancestor_id = ancestor["id"].as_str().expect("id").to_owned();

    // A private ancestor is bindable.
    let (status, bound) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{bound}");
    let bound_id = bound["id"].as_str().expect("id").to_owned();

    // Flip the ancestor to `enforce`; the same bind is now a 403.
    let enforced = {
        let mut draft = derived_upstream("api.openai.com");
        draft["plugins"] = json!({"sharing": "enforce"});
        draft
    };
    let (status, body) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{ancestor_id}"),
            ctx(OTHER),
            Some(&enforced.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["plugins"]["sharing"], json!("enforce"), "{body}");

    let (status, conflict) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{conflict}");
    assert_eq!(
        conflict["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.tenancy.bind_forbidden.v1"),
        "{conflict}"
    );
    assert_eq!(conflict["context"]["alias"], json!("api.openai.com"));

    // The same denial on the replace path: a `PUT` that keeps the alias cannot
    // sidestep the ancestor's `enforce` (F8).
    let (status, replaced) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{bound_id}"),
            ctx(TENANT),
            Some(
                &{
                    let mut draft = derived_upstream("api.openai.com");
                    draft["tags"] = json!(["rebound"]);
                    draft
                }
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{replaced}");
    assert_eq!(
        replaced["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.tenancy.bind_forbidden.v1"),
        "{replaced}"
    );

    // The ancestor itself is unaffected: `vendor.com` was never claimed, so the
    // descendant may still bind it.
    let (status, unclaimed) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("vendor.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{unclaimed}");
}

/// Hierarchy that resolves the nearest parent and a further ancestor, for the
/// two-level bind-rule test.
struct TwoLevelHierarchy {
    /// Ancestor of the calling tenant (`TENANT`).
    parent: Uuid,
    /// Ancestor of `parent`.
    grandparent: Uuid,
}

#[async_trait]
impl TenantHierarchy for TwoLevelHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        if tenant == self.parent {
            vec![self.grandparent]
        } else {
            vec![self.parent, self.grandparent]
        }
    }
}

#[tokio::test]
async fn the_bind_rule_walks_the_whole_ancestor_chain() {
    let parent = Uuid::from_u128(0x31);
    let grandparent = Uuid::from_u128(0x41);
    let mut app = build_app(
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        Arc::new(TwoLevelHierarchy {
            parent,
            grandparent,
        }),
    );

    // The grandparent claims the alias and enforces it.
    let (status, claimed) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(grandparent),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{claimed}");
    let grandparent_id = claimed["id"].as_str().expect("id").to_owned();
    let mut enforced = derived_upstream("api.openai.com");
    enforced["plugins"] = json!({"sharing": "enforce"});
    let (status, body) = call(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{grandparent_id}"),
            ctx(grandparent),
            Some(&enforced.to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");

    // The parent, which owns nothing, cannot bind it either: the rule walks the
    // whole chain, not only the nearest ancestor.
    let (status, denied) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(parent),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{denied}");

    // Nor can the leaf.
    let (status, denied) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("api.openai.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(
        denied["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.tenancy.bind_forbidden.v1"),
        "{denied}"
    );

    // A private ancestor does not block: the leaf binds over it.
    let (status, ancestor) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(grandparent),
            Some(&derived_upstream("private.example.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{ancestor}");
    let (status, bound) = call(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            ctx(TENANT),
            Some(&derived_upstream("private.example.com").to_string()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{bound}");
}

/// Keeps `OagwError` and `SharingMode` referenced for the doc-test surface of
/// the module (they are asserted on in the unit tests).
#[test]
fn the_error_and_sharing_types_are_exported() {
    let error = OagwError::not_found("probe");
    assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(SharingMode::default(), SharingMode::Private);
}
