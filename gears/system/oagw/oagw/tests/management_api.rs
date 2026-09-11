//! Management API: routes, bodies, status codes and validation, exercised
//! through the real router the gear registers.

mod common;

use common::{Fixture, HTTP_PROTOCOL, empty_request, json_request};
use http::StatusCode;
use oagw::test_utils::HarnessBuilder;
use serde_json::json;
use uuid::Uuid;

fn hostname_upstream() -> serde_json::Value {
    json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
        "protocol": HTTP_PROTOCOL,
    })
}

// -- Routes are registered gear-relative ------------------------------------

#[tokio::test]
async fn the_management_api_is_served_under_the_gear_relative_prefix() {
    let fixture = Fixture::new();
    let (status, _) = fixture.get("/oagw/v1/upstreams").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_gear_does_not_repeat_the_gateway_prefix_itself() {
    // The api-gateway nests this router under its own `prefix_path`; a gear
    // that also registered `/api/...` would answer on the wrong path.
    let fixture = Fixture::new();
    let response = fixture.send(empty_request("GET", "/api/oagw/v1/upstreams")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// -- Upstream CRUD ----------------------------------------------------------

#[tokio::test]
async fn creating_an_upstream_answers_201_with_the_resource_and_a_location() {
    let fixture = Fixture::new();
    let response = fixture
        .send(json_request("POST", "/oagw/v1/upstreams", &hostname_upstream()))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .expect("a Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let (_, body) = common::split(response).await;

    let id = body["id"].as_str().expect("an id");
    assert_eq!(location, format!("/oagw/v1/upstreams/{id}"));
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["protocol"], HTTP_PROTOCOL);
}

#[tokio::test]
async fn an_upstream_can_be_read_back_by_uuid_and_by_gts_identifier() {
    let fixture = Fixture::new();
    let created = fixture.create_upstream(hostname_upstream()).await;
    let id = created["id"].as_str().unwrap();

    let (status, by_uuid) = fixture.get(&format!("/oagw/v1/upstreams/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_uuid["id"], id);

    let (status, by_gts) = fixture
        .get(&format!("/oagw/v1/upstreams/gts.cf.core.oagw.upstream.v1~{id}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_gts["id"], id);
}

#[tokio::test]
async fn a_malformed_identifier_is_a_validation_error() {
    let fixture = Fixture::new();
    let (status, body) = fixture.get("/oagw/v1/upstreams/not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn a_missing_upstream_is_a_404_route_not_found() {
    let fixture = Fixture::new();
    let (status, body) = fixture
        .get(&format!("/oagw/v1/upstreams/{}", Uuid::new_v4()))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn replacing_an_upstream_answers_200_with_the_new_representation() {
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
            "protocol": HTTP_PROTOCOL,
            "tags": ["llm"],
        }))
        .await;
    let id = created["id"].as_str().unwrap();

    let (status, replaced) = fixture
        .put_json(&format!("/oagw/v1/upstreams/{id}"), hostname_upstream())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replaced["id"], id);
    // A full replacement clears omitted optional fields.
    assert_eq!(replaced["tags"], json!([]));
}

#[tokio::test]
async fn deleting_an_upstream_answers_204_and_removes_it() {
    let fixture = Fixture::new();
    let created = fixture.create_upstream(hostname_upstream()).await;
    let id = created["id"].as_str().unwrap();

    assert_eq!(
        fixture.delete(&format!("/oagw/v1/upstreams/{id}")).await,
        StatusCode::NO_CONTENT
    );
    let (status, _) = fixture.get(&format!("/oagw/v1/upstreams/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_duplicate_alias_answers_409() {
    let fixture = Fixture::new();
    fixture.create_upstream(hostname_upstream()).await;
    let (status, _) = fixture.post_json("/oagw/v1/upstreams", hostname_upstream()).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// -- Validation -------------------------------------------------------------

#[tokio::test]
async fn a_missing_body_is_a_validation_error() {
    let fixture = Fixture::new();
    let response = fixture.send(empty_request("POST", "/oagw/v1/upstreams")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_json_is_a_400_not_a_422() {
    let fixture = Fixture::new();
    let request = http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{ not json"))
        .unwrap();
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (_, body) = common::split(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn a_missing_required_field_is_a_validation_error() {
    let fixture = Fixture::new();
    let (status, _) = fixture
        .post_json("/oagw/v1/upstreams", json!({"protocol": HTTP_PROTOCOL}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_member_is_rejected_as_the_schema_requires() {
    // `schemas/upstream.v1.schema.json` sets `additionalProperties: false`.
    let fixture = Fixture::new();
    let mut body = hostname_upstream();
    body["not_a_field"] = json!(true);
    let (status, _) = fixture.post_json("/oagw/v1/upstreams", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_get_response_can_be_put_straight_back() {
    // `id` is read-only but accepted, so a client can round-trip a resource.
    let fixture = Fixture::new();
    let created = fixture.create_upstream(hostname_upstream()).await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, _) = fixture
        .put_json(&format!("/oagw/v1/upstreams/{id}"), created)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_http_scheme_is_accepted_by_the_management_api() {
    // `allow_http_upstream` governs whether a plaintext connection is dialled,
    // not which schemes the API admits.
    let fixture = Fixture::new();
    let (status, body) = fixture
        .post_json(
            "/oagw/v1/upstreams",
            json!({
                "alias": "local",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 80}]},
                "protocol": HTTP_PROTOCOL,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "http");
}

#[tokio::test]
async fn an_unknown_scheme_is_still_rejected() {
    let fixture = Fixture::new();
    let (status, _) = fixture
        .post_json(
            "/oagw/v1/upstreams",
            json!({
                "alias": "weird",
                "server": {"endpoints": [{"scheme": "gopher", "host": "127.0.0.1", "port": 70}]},
                "protocol": HTTP_PROTOCOL,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// -- Listing ----------------------------------------------------------------

#[tokio::test]
async fn listing_returns_an_envelope_with_items_and_a_total() {
    let fixture = Fixture::new();
    fixture.create_upstream(hostname_upstream()).await;
    fixture
        .create_upstream(json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.stripe.com", "port": 443}]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/upstreams").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn listing_supports_the_documented_query_parameters() {
    let fixture = Fixture::new();
    for host in ["api.openai.com", "api.stripe.com", "api.twilio.com"] {
        fixture
            .create_upstream(json!({
                "server": {"endpoints": [{"scheme": "https", "host": host, "port": 443}]},
                "protocol": HTTP_PROTOCOL,
            }))
            .await;
    }

    let (_, filtered) = fixture
        .get("/oagw/v1/upstreams?%24filter=alias%20eq%20%27api.stripe.com%27")
        .await;
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["items"][0]["alias"], "api.stripe.com");

    let (_, page) = fixture
        .get("/oagw/v1/upstreams?%24orderby=alias&%24top=1&%24skip=1")
        .await;
    assert_eq!(page["total"], 3, "total counts matches before paging");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["alias"], "api.stripe.com");

    let (_, projected) = fixture.get("/oagw/v1/upstreams?%24select=id,alias").await;
    let first = projected["items"][0].as_object().unwrap();
    assert_eq!(first.len(), 2);
}

#[tokio::test]
async fn a_malformed_query_parameter_is_a_validation_error() {
    let fixture = Fixture::new();
    let (status, _) = fixture.get("/oagw/v1/upstreams?%24top=lots").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// -- Tenant scoping ---------------------------------------------------------

#[tokio::test]
async fn another_tenant_can_neither_see_nor_touch_the_resource() {
    let fixture = Fixture::new();
    let created = fixture.create_upstream(hostname_upstream()).await;
    let id = created["id"].as_str().unwrap();
    let intruder = Uuid::new_v4();

    let response = fixture
        .send_as(intruder, empty_request("GET", &format!("/oagw/v1/upstreams/{id}")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = fixture
        .send_as(
            intruder,
            empty_request("DELETE", &format!("/oagw/v1/upstreams/{id}")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = fixture
        .send_as(intruder, empty_request("GET", "/oagw/v1/upstreams"))
        .await;
    let (_, body) = common::split(response).await;
    assert_eq!(body["total"], 0);
}

// -- Routes -----------------------------------------------------------------

#[tokio::test]
async fn creating_a_route_answers_201_and_normalizes_the_match() {
    let fixture = Fixture::new();
    let upstream = fixture.create_upstream(hostname_upstream()).await;
    let route = fixture
        .create_route(json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": ["get"], "path": "v1/chat/"}},
        }))
        .await;

    assert_eq!(route["upstream_id"], upstream["id"]);
    assert_eq!(route["match"]["http"]["path"], "/v1/chat");
    assert_eq!(route["match"]["http"]["methods"], json!(["GET"]));
    assert_eq!(route["match"]["http"]["path_suffix_mode"], "append");
    assert_eq!(route["enabled"], true);
}

#[tokio::test]
async fn a_route_on_an_unknown_upstream_is_a_validation_error() {
    let fixture = Fixture::new();
    let (status, _) = fixture
        .post_json(
            "/oagw/v1/routes",
            json!({
                "upstream_id": Uuid::new_v4(),
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_duplicate_match_rule_answers_409() {
    let fixture = Fixture::new();
    let upstream = fixture.create_upstream(hostname_upstream()).await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    fixture.create_route(body.clone()).await;
    let (status, _) = fixture.post_json("/oagw/v1/routes", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn deleting_an_upstream_takes_its_routes_with_it() {
    let fixture = Fixture::new();
    let upstream = fixture.create_upstream(hostname_upstream()).await;
    fixture
        .create_route(json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    fixture
        .delete(&format!(
            "/oagw/v1/upstreams/{}",
            upstream["id"].as_str().unwrap()
        ))
        .await;

    let (_, routes) = fixture.get("/oagw/v1/routes").await;
    assert_eq!(routes["total"], 0);
}

// -- Plugins ----------------------------------------------------------------

#[tokio::test]
async fn a_custom_plugin_is_created_read_and_deleted() {
    let fixture = Fixture::new();
    let (status, plugin) = fixture
        .post_json(
            "/oagw/v1/plugins",
            json!({
                "name": "redact_pii",
                "plugin_type": "transform",
                "phases": ["on_response"],
                "config_schema": {"type": "object"},
                "source_code": "def on_response(ctx):\n    return ctx.next()\n",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let id = plugin["id"].as_str().unwrap().to_owned();
    assert_eq!(
        plugin["gts_id"],
        format!("gts.cf.core.oagw.transform_plugin.v1~{id}")
    );
    // A listing must not carry script bodies.
    assert!(plugin.get("source_code").is_none());

    let (status, source) = fixture
        .get(&format!("/oagw/v1/plugins/{id}/source"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(source["source_code"].as_str().unwrap().contains("on_response"));

    assert_eq!(
        fixture.delete(&format!("/oagw/v1/plugins/{id}")).await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn a_plugin_that_is_still_bound_cannot_be_deleted() {
    let fixture = Fixture::new();
    let (_, plugin) = fixture
        .post_json(
            "/oagw/v1/plugins",
            json!({
                "name": "guard",
                "plugin_type": "guard",
                "source_code": "def on_request(ctx): pass",
            }),
        )
        .await;
    let id = plugin["id"].as_str().unwrap().to_owned();

    fixture
        .create_upstream(json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
            "protocol": HTTP_PROTOCOL,
            "plugins": {"items": [format!("gts.cf.core.oagw.guard_plugin.v1~{id}")]},
        }))
        .await;

    let response = fixture
        .send(empty_request("DELETE", &format!("/oagw/v1/plugins/{id}")))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let (_, body) = common::split(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    assert_eq!(body["referenced_by"]["upstreams"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_duplicate_plugin_name_answers_409() {
    let fixture = Fixture::new();
    let body = json!({
        "name": "guard",
        "plugin_type": "guard",
        "source_code": "def on_request(ctx): pass",
    });
    let (status, _) = fixture.post_json("/oagw/v1/plugins", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = fixture.post_json("/oagw/v1/plugins", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn plugins_are_immutable_so_there_is_no_replace_verb() {
    let fixture = Fixture::new();
    let (_, plugin) = fixture
        .post_json(
            "/oagw/v1/plugins",
            json!({
                "name": "guard",
                "plugin_type": "guard",
                "source_code": "def on_request(ctx): pass",
            }),
        )
        .await;
    let id = plugin["id"].as_str().unwrap();

    let response = fixture
        .send(json_request(
            "PUT",
            &format!("/oagw/v1/plugins/{id}"),
            &json!({"name": "guard"}),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// -- Errors carry the gateway marker ----------------------------------------

#[tokio::test]
async fn every_management_answer_states_who_produced_it() {
    let fixture = Fixture::new();
    let response = fixture
        .send(json_request("POST", "/oagw/v1/upstreams", &hostname_upstream()))
        .await;
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );

    let response = fixture.send(empty_request("GET", "/oagw/v1/upstreams/nope")).await;
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
}

// -- Hierarchy --------------------------------------------------------------

#[tokio::test]
async fn an_ancestor_upstream_stays_invisible_to_the_management_api() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let fixture = Fixture::with_builder(HarnessBuilder::new().with_tenant_parent(child, parent));

    let response = fixture
        .send_as(parent, json_request("POST", "/oagw/v1/upstreams", &hostname_upstream()))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let (_, created) = common::split(response).await;
    let id = created["id"].as_str().unwrap();

    let response = fixture
        .send_as(child, empty_request("GET", &format!("/oagw/v1/upstreams/{id}")))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
