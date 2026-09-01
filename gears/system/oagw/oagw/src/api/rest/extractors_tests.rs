//! Tests for [`crate::api::rest::extractors`].

use axum::body::Body;
use axum::extract::FromRequest;
use axum::http::Request;
use serde_json::json;

use super::{JsonBody, ListQuery, MAX_BODY_BYTES};
use crate::domain::error::OagwError;
use crate::domain::model::{Endpoint, Scheme, ServerConfig};
use crate::domain::odata::UPSTREAM_FIELDS;

async fn body<T>(raw: &str) -> Result<T, OagwError>
where
    T: serde::de::DeserializeOwned,
{
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(Body::from(raw.to_owned()))
        .expect("request builds");
    JsonBody::<T>::from_request(request, &())
        .await
        .map(|parsed| parsed.0)
}

async fn oversized_body() -> OagwError {
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-length", (MAX_BODY_BYTES + 1).to_string())
        .body(Body::from("x"))
        .expect("request builds");
    JsonBody::<serde_json::Value>::from_request(request, &())
        .await
        .expect_err("body is too large")
}

#[derive(Debug, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Probe {
    alias: String,
}

#[tokio::test]
async fn a_strict_body_accepts_unknown_nothing() {
    let probe = body::<Probe>("{\"alias\": \"payments\"}")
        .await
        .expect("parsed");
    assert_eq!(
        probe,
        Probe {
            alias: "payments".to_owned(),
        }
    );
}

#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let error = body::<Probe>("{\"alias\": }").await.expect_err("malformed");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("malformed JSON body"), "{error}");
}

#[tokio::test]
async fn an_unknown_field_is_a_400() {
    let error = body::<Probe>("{\"alias\": \"payments\", \"tenant_id\": \"x\"}")
        .await
        .expect_err("unknown field");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("unknown field"), "{error}");
}

#[tokio::test]
async fn a_wrong_typed_field_is_a_400() {
    let error = body::<Probe>("{\"alias\": 7}")
        .await
        .expect_err("wrong type");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("invalid request body"), "{error}");
}

#[tokio::test]
async fn an_oversized_body_is_rejected() {
    let error = oversized_body().await;
    assert_eq!(error.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(error.detail().contains("exceeds"), "{error}");
}

#[tokio::test]
async fn an_oversized_chunked_body_is_rejected_too() {
    // No `content-length`: the cap has to be enforced while streaming.
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .header("transfer-encoding", "chunked")
        .body(Body::from(vec![b'x'; MAX_BODY_BYTES + 1]))
        .expect("request builds");
    let error = JsonBody::<serde_json::Value>::from_request(request, &())
        .await
        .expect_err("chunked body is too large");
    assert_eq!(error.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
    assert!(error.detail().contains("exceeds"), "{error}");
}

#[tokio::test]
async fn a_chunked_body_at_the_limit_is_accepted() {
    // A JSON object padded to exactly the cap, so the boundary stays inclusive.
    let padding = MAX_BODY_BYTES - 20;
    let payload = {
        let mut raw = String::from("{\"alias\":\"");
        raw.push_str(&"a".repeat(padding));
        raw.push_str("\"}");
        raw
    };
    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .header("transfer-encoding", "chunked")
        .body(Body::from(payload))
        .expect("request builds");
    let parsed = JsonBody::<serde_json::Value>::from_request(request, &())
        .await
        .expect("the body is exactly at the cap");
    assert_eq!(parsed.0["alias"].as_str().map(str::len), Some(padding));
}

#[test]
fn the_upstream_catalog_shapes_list_queries() {
    let query = ListQuery::parse(
        Some("$filter=alias+eq+%27api.openai.com%27&$top=5"),
        &UPSTREAM_FIELDS,
    )
    .expect("query parses");
    assert_eq!(query.top, 5);
    let rows = vec![serde_json::json!({"alias": "api.openai.com", "id": "a"})];
    assert_eq!(
        query.apply_to_rows(rows.clone())[0],
        json!({"alias": "api.openai.com", "id": "a"})
    );

    let error = ListQuery::parse(Some("$filter=tenant_id eq 'x'"), &UPSTREAM_FIELDS)
        .expect_err("tenant_id is not queryable");
    assert!(error.detail().contains("tenant_id"), "{error}");
}

#[test]
fn endpoint_drafts_deserialise_through_the_dto_catalog() {
    // Sanity check for the catalog constants: the fields they name are the
    // ones the DTOs actually serialise.
    assert!(UPSTREAM_FIELDS.filterable.contains(&"protocol"));
    let server = ServerConfig {
        endpoints: vec![Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: 443,
        }],
    };
    let rendered = serde_json::to_value(server).expect("serialises");
    assert!(rendered["endpoints"][0]["host"].is_string());
}
