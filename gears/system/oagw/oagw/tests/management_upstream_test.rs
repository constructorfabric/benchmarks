//! Upstream CRUD contract (T015, T016).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

const OAGW: &str = "/oagw/v1/upstreams";

#[tokio::test]
async fn a_create_answers_201_with_a_location_and_a_body() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "enabled": true,
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .expect("a Location header is returned");
    assert!(location.starts_with("/oagw/v1/upstreams/"), "{location}");
    let body = read_json(response).await;
    assert_eq!(body["alias"], "api.openai.com", "the alias is derived");
    assert_eq!(body["server"]["endpoints"][0]["host"], "api.openai.com");
    assert!(body["id"].as_str().is_some());
}

#[tokio::test]
async fn an_alias_override_of_a_derived_alias_is_rejected_with_400() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "not-the-derivation",
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = read_json(response).await;
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn a_bad_host_is_rejected_with_400() {
    let harness = Harness::default_gear();
    for host in ["not a host", "", "bad_", "-leading"] {
        let response = harness
            .send(harness.request(
                "POST",
                OAGW,
                Some(json!({
                    "server": { "endpoints": [ { "scheme": "https", "host": host, "port": 443 } ] },
                    "protocol": "http"
                })),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "host `{host}`");
    }
}

#[tokio::test]
async fn an_ip_endpoint_requires_an_explicit_alias() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_explicit_alias_is_accepted_for_ip_endpoints() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "shared-ip",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(read_json(response).await["alias"], "shared-ip");
}

#[tokio::test]
async fn an_invalid_tag_is_rejected_with_400() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "tags": ["Not A Valid Tag"],
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cors_allow_credentials_with_a_wildcard_is_rejected() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "http",
                "cors": {
                    "enabled": true,
                    "allow_credentials": true,
                    "allowed_origins": ["*"],
                    "allowed_methods": ["GET"]
                }
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_duplicate_tenant_alias_is_a_conflict() {
    let harness = Harness::default_gear();
    let first = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "explicit-one",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let id = read_json(first).await["id"].as_str().unwrap_or_default().to_string();

    let second = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "explicit-one",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.6", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body = read_json(second).await;
    assert_eq!(body["status"], 409);

    // Deleting the row frees the alias.
    let deleted = harness
        .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let recreated = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "explicit-one",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.6", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(recreated.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_put_replaces_the_row_and_clears_omitted_optionals() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "tags": ["first"],
                "alias": "tagged-one",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let updated = harness
        .send(harness.request(
            "PUT",
            &format!("{OAGW}/{id}"),
            Some(json!({
                "enabled": false,
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let body = read_json(updated).await;
    assert_eq!(body["enabled"], false, "the replacement is taken wholesale");
    // The schema leaves `tags` out when the list is empty; the old value must
    // be gone either way.
    let tags = body["tags"].as_array().cloned().unwrap_or_default();
    assert!(tags.is_empty(), "omitted fields are cleared: {body}");

    let fetched = harness
        .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
        .await;
    assert_eq!(read_json(fetched).await["enabled"], false);
}

#[tokio::test]
async fn a_delete_answers_204_and_the_row_is_gone() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "alias": "ip-249",
                "protocol": "http"
            })),
        ))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(
        harness
            .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        harness
            .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_unknown_id_is_not_found() {
    let harness = Harness::default_gear();
    for method in ["GET", "PUT", "DELETE"] {
        let request = match method {
            "GET" => harness.request("GET", &format!("{OAGW}/{}", Uuid::new_v4()), None),
            "PUT" => harness.request(
                "PUT",
                &format!("{OAGW}/{}", Uuid::new_v4()),
                Some(json!({
                    "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                    "alias": "ip-281",
                    "protocol": "http"
                })),
            ),
            _ => harness.request("DELETE", &format!("{OAGW}/{}", Uuid::new_v4()), None),
        };
        assert_eq!(harness.send(request).await.status(), StatusCode::NOT_FOUND, "{method}");
    }
}

#[tokio::test]
async fn another_tenant_s_row_is_invisible() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                "alias": "ip-299",
                "protocol": "http"
            })),
        ))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let foreign = harness.request_for("GET", &format!("{OAGW}/{id}"), None, other_tenant());
    assert_eq!(harness.send(foreign).await.status(), StatusCode::NOT_FOUND);

    let foreign_delete = harness.request_for("DELETE", &format!("{OAGW}/{id}"), None, other_tenant());
    assert_eq!(harness.send(foreign_delete).await.status(), StatusCode::NOT_FOUND);

    // The list only ever returns the caller's tenant.
    harness
        .send(harness.request_for(
            "POST",
            OAGW,
            Some(json!({
                "alias": "foreign-row",
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.9", "port": 443 } ] },
                "protocol": "http"
            })),
            other_tenant(),
        ))
        .await;
    let listed = harness
        .send(harness.request("GET", OAGW, None))
        .await;
    let body = read_json(listed).await;
    let values = body["value"].as_array().cloned().unwrap_or_default();
    assert!(values.len() == 1, "only the caller's rows: {values:?}");
    assert_eq!(values[0]["id"], id);
}

#[tokio::test]
async fn the_list_honours_odata_parameters() {
    let harness = Harness::default_gear();
    for index in 0..4 {
        harness
            .send(harness.request(
                "POST",
                OAGW,
                Some(json!({
                    "alias": format!("ip-{index}"),
                    "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5", "port": 443 } ] },
                    "protocol": "http"
                })),
            ))
            .await;
    }
    let top = read_json(harness.send(harness.request("GET", &format!("{OAGW}?$top=2"), None)).await).await;
    assert_eq!(top["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(top["count"], 4);

    let skip = read_json(harness.send(harness.request("GET", &format!("{OAGW}?$top=2&$skip=2"), None)).await).await;
    assert_eq!(skip["value"].as_array().map(Vec::len), Some(2));

    let filtered = read_json(
        harness
            .send(harness.request(
                "GET",
                &format!("{OAGW}?$filter=alias%20eq%20'ip-1'"),
                None,
            ))
            .await,
    )
    .await;
    assert!(filtered["value"].is_array());

    let bad_top = read_json(harness.send(harness.request("GET", &format!("{OAGW}?$top=1000"), None)).await).await;
    assert_eq!(bad_top["value"].as_array().map(Vec::len), Some(4), "`$top` clamps to 100");
}

#[tokio::test]
async fn an_http_endpoint_scheme_is_accepted() {
    // `http` is a legal scheme value; `allow_http_upstream` only governs
    // whether a plaintext connection is actually made.
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "http", "host": "stub.internal", "port": 8081 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "the scheme field is a data question");
    let id = read_json(response).await["id"].as_str().unwrap_or_default().to_string();

    let fetched = harness
        .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
        .await;
    let body = read_json(fetched).await;
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "http");
    assert_eq!(body["server"]["endpoints"][0]["port"], 8081);
}

#[tokio::test]
async fn an_unknown_upstream_id_route_cannot_be_bound() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&Uuid::new_v4().to_string(), "/v1/x", &["GET"])),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_replace_cannot_move_an_upstream_to_another_alias() {
    // US1/AC6, FR-009: the alias is a stable routing key derived from the
    // endpoints at create time; a replace that would rename it is refused and
    // the stored alias is untouched.
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let response = harness
        .send(harness.request(
            "PUT",
            &format!("{OAGW}/{id}"),
            Some(json!({
                "enabled": true,
                "server": { "endpoints": [ { "scheme": "https", "host": "api.anthropic.com", "port": 443 } ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let fetched = harness
        .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
        .await;
    let body = read_json(fetched).await;
    assert_eq!(body["alias"], "api.openai.com", "the stored alias is unchanged");
    assert_eq!(body["server"]["endpoints"][0]["host"], "api.openai.com");
}

#[tokio::test]
async fn a_pool_whose_endpoints_disagree_on_port_or_scheme_is_rejected() {
    // FR-011, Edge Cases §1: a pool is a uniform set of endpoints — it must
    // agree on scheme and port, so a mismatched create is refused.
    let harness = Harness::default_gear();
    let mismatched_port = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "mismatched-port",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 },
                    { "scheme": "https", "host": "alt.openai.com", "port": 8443 }
                ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(
        mismatched_port.status(),
        StatusCode::BAD_REQUEST,
        "a pool must agree on its port"
    );
    assert_eq!(read_json(mismatched_port).await["status"], 400);

    let mismatched_scheme = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "alias": "mismatched-scheme",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 },
                    { "scheme": "http", "host": "alt.openai.com", "port": 443 }
                ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(
        mismatched_scheme.status(),
        StatusCode::BAD_REQUEST,
        "a pool must agree on its scheme"
    );
    assert_eq!(read_json(mismatched_scheme).await["status"], 400);

    // A pool that does agree is still accepted, so the refusals above are the
    // mismatch, not the shape.
    let uniform = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 },
                    { "scheme": "https", "host": "alt.openai.com", "port": 443 }
                ] },
                "protocol": "http"
            })),
        ))
        .await;
    assert_eq!(uniform.status(), StatusCode::CREATED);
    // A derivable pool derives its alias from the shared registrable suffix.
    assert_eq!(read_json(uniform).await["alias"], "openai.com");
}
