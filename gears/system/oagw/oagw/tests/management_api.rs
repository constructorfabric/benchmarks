//! The management REST surface over a real socket: paths, status codes,
//! bodies, validation and the OData list parameters.

mod common;

use common::{Fixture, assert_gateway_problem, delete, get, post_json, put_json, send};
use http::{Method, StatusCode};
use oagw::domain::gts_helpers::{self, errors};
use serde_json::{Value, json};

fn upstream_body(host: &str, port: u16, alias: Option<&str>) -> Value {
    let mut body = json!({
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
        "protocol": gts_helpers::PROTOCOL_HTTP,
    });
    if let Some(alias) = alias
        && let Some(object) = body.as_object_mut()
    {
        object.insert("alias".to_owned(), json!(alias));
    }
    body
}

#[tokio::test]
async fn the_management_api_is_mounted_gear_relative() {
    let fx = Fixture::start().await;
    // The api-gateway supplies `/api`; the gear must not repeat it.
    let relative = get(&fx.api_url("/upstreams")).await;
    assert_eq!(relative.status, StatusCode::OK);

    let prefixed = get(&format!(
        "{}/api/oagw/v1/upstreams",
        fx.gateway.base_url
    ))
    .await;
    assert_eq!(
        prefixed.status,
        StatusCode::NOT_FOUND,
        "the gear does not own the operator gateway's prefix"
    );
}

#[tokio::test]
async fn upstream_create_read_replace_delete() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();

    let created = post_json(
        &fx.api_url("/upstreams"),
        &upstream_body(&host, port, Some("mock")),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert!(
        created.header("location").is_some(),
        "a create points at the new resource"
    );
    let body = created.json();
    assert_eq!(body["alias"], json!("mock"));
    assert_eq!(body["enabled"], json!(true));
    assert!(
        body.get("tenant_id").is_none(),
        "the entity schema is additionalProperties:false and has no tenant_id"
    );
    let id = body["id"].as_str().expect("id").to_owned();

    let fetched = get(&fx.api_url(&format!("/upstreams/{id}"))).await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.json()["id"], json!(id));

    // The same resource is addressable by its anonymous GTS identifier.
    let gts_id = format!("{}{id}", gts_helpers::UPSTREAM_TYPE);
    let by_gts = get(&fx.api_url(&format!("/upstreams/{gts_id}"))).await;
    assert_eq!(by_gts.status, StatusCode::OK);

    let mut replacement = upstream_body(&host, port, Some("mock"));
    if let Some(object) = replacement.as_object_mut() {
        object.insert("tags".to_owned(), json!(["llm", "internal"]));
    }
    let replaced = put_json(&fx.api_url(&format!("/upstreams/{id}")), &replacement).await;
    assert_eq!(replaced.status, StatusCode::OK);
    assert_eq!(replaced.json()["tags"], json!(["internal", "llm"]));

    let deleted = delete(&fx.api_url(&format!("/upstreams/{id}"))).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        get(&fx.api_url(&format!("/upstreams/{id}"))).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_hostname_pool_derives_its_alias_and_rejects_an_override() {
    let fx = Fixture::start().await;

    let derived = post_json(
        &fx.api_url("/upstreams"),
        &json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
        }),
    )
    .await;
    assert_eq!(derived.status, StatusCode::CREATED);
    assert_eq!(derived.json()["alias"], json!("api.openai.com"));

    let overridden = post_json(
        &fx.api_url("/upstreams"),
        &json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.anthropic.com", "port": 443 } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "alias": "claude",
        }),
    )
    .await;
    assert_gateway_problem(&overridden, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_plaintext_endpoint_is_accepted_by_the_management_api() {
    let fx = Fixture::start().await;
    // Note 2 of the wire contract: the scheme is a legal field value; only
    // the *connection* is gated by `allow_http_upstream`.
    let created = post_json(
        &fx.api_url("/upstreams"),
        &json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 80 } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "alias": "plaintext",
        }),
    )
    .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "an http endpoint must not be rejected at create time; body: {}",
        String::from_utf8_lossy(&created.body)
    );
    assert_eq!(created.json()["server"]["endpoints"][0]["scheme"], json!("http"));
}

#[tokio::test]
async fn a_duplicate_alias_conflicts() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let body = upstream_body(&host, port, Some("mock"));

    assert_eq!(
        post_json(&fx.api_url("/upstreams"), &body).await.status,
        StatusCode::CREATED
    );
    let conflict = post_json(&fx.api_url("/upstreams"), &body).await;
    assert_gateway_problem(&conflict, StatusCode::CONFLICT, errors::CONFLICT);
}

#[tokio::test]
async fn an_unknown_field_is_a_validation_error() {
    let fx = Fixture::start().await;
    let res = post_json(
        &fx.api_url("/upstreams"),
        &json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "surprise": true,
        }),
    )
    .await;
    assert!(
        res.status.is_client_error(),
        "an unknown member must not be silently ignored"
    );
}

#[tokio::test]
async fn route_create_read_replace_delete() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let upstream = post_json(
        &fx.api_url("/upstreams"),
        &upstream_body(&host, port, Some("mock")),
    )
    .await
    .json();
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let created = post_json(
        &fx.api_url("/routes"),
        &json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let route = created.json();
    assert_eq!(route["upstream_id"], json!(upstream_id));
    assert_eq!(route["match"]["http"]["path"], json!("/v1/models"));
    assert_eq!(
        route["match"]["http"]["path_suffix_mode"],
        json!("append"),
        "the schema default is materialized in the response"
    );
    let route_id = route["id"].as_str().expect("id").to_owned();

    let replaced = put_json(
        &fx.api_url(&format!("/routes/{route_id}")),
        &json!({
            "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/models" } },
            "priority": 5,
        }),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK);
    let replaced = replaced.json();
    assert_eq!(replaced["priority"], json!(5));
    assert_eq!(
        replaced["upstream_id"],
        json!(upstream_id),
        "upstream_id is immutable"
    );

    assert_eq!(
        delete(&fx.api_url(&format!("/routes/{route_id}"))).await.status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn a_route_on_an_unknown_upstream_is_a_validation_error() {
    let fx = Fixture::start().await;
    let res = post_json(
        &fx.api_url("/routes"),
        &json!({
            "upstream_id": uuid::Uuid::new_v4().to_string(),
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        }),
    )
    .await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_duplicate_match_rule_conflicts() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let upstream_id = post_json(
        &fx.api_url("/upstreams"),
        &upstream_body(&host, port, Some("mock")),
    )
    .await
    .json()["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
    });
    assert_eq!(
        post_json(&fx.api_url("/routes"), &body).await.status,
        StatusCode::CREATED
    );
    let conflict = post_json(&fx.api_url("/routes"), &body).await;
    assert_gateway_problem(&conflict, StatusCode::CONFLICT, errors::CONFLICT);
}

#[tokio::test]
async fn plugin_lifecycle_including_source_and_in_use_conflict() {
    let fx = Fixture::start().await;

    let created = post_json(
        &fx.api_url("/plugins"),
        &json!({
            "name": "request_validator",
            "description": "Validates request headers",
            "plugin_type": "guard",
            "phases": ["on_request"],
            "config_schema": { "type": "object" },
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let plugin = created.json();
    let plugin_gts = plugin["id"].as_str().expect("id").to_owned();
    let plugin_uuid = plugin["uuid"].as_str().expect("uuid").to_owned();
    assert!(plugin_gts.starts_with(gts_helpers::GUARD_PLUGIN_TYPE));
    assert!(
        plugin.get("source_code").is_none(),
        "the body is served by the dedicated source endpoint"
    );

    let source = get(&fx.api_url(&format!("/plugins/{plugin_gts}/source"))).await;
    assert_eq!(source.status, StatusCode::OK);
    assert!(
        source.json()["source_code"]
            .as_str()
            .is_some_and(|s| s.contains("on_request")),
        "the stored body is returned verbatim"
    );

    // A plugin has no PUT: definitions are immutable.
    let no_put = put_json(
        &fx.api_url(&format!("/plugins/{plugin_uuid}")),
        &json!({ "name": "renamed" }),
    )
    .await;
    assert_eq!(no_put.status, StatusCode::METHOD_NOT_ALLOWED);

    // Bind it, then try to delete.
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let mut upstream = upstream_body(&host, port, Some("mock"));
    if let Some(object) = upstream.as_object_mut() {
        object.insert(
            "plugins".to_owned(),
            json!({ "items": [ { "plugin_ref": plugin_gts } ] }),
        );
    }
    assert_eq!(
        post_json(&fx.api_url("/upstreams"), &upstream).await.status,
        StatusCode::CREATED
    );

    let in_use = delete(&fx.api_url(&format!("/plugins/{plugin_uuid}"))).await;
    assert_gateway_problem(&in_use, StatusCode::CONFLICT, errors::PLUGIN_IN_USE);
    let body = in_use.json();
    assert!(
        body["context"]["referenced_by"]["upstreams"]
            .as_array()
            .is_some_and(|a| a.len() == 1),
        "the conflict names what still references the plugin"
    );
}

#[tokio::test]
async fn a_catalog_only_plugin_identifier_cannot_be_bound() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let mut body = upstream_body(&host, port, Some("mock"));
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "plugins".to_owned(),
            json!({ "items": [ gts_helpers::CORS_GUARD_PLUGIN_ID ] }),
        );
    }
    let res = post_json(&fx.api_url("/upstreams"), &body).await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_plugin_chain_accepts_both_binding_spellings() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let mut body = upstream_body(&host, port, Some("mock"));
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "plugins".to_owned(),
            json!({ "items": [
                gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
                { "plugin_ref": gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                  "config": { "required_request_headers": "x-correlation-id" } }
            ] }),
        );
    }
    let created = post_json(&fx.api_url("/upstreams"), &body).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let items = created.json()["plugins"]["items"].clone();
    assert_eq!(items.as_array().map(Vec::len), Some(2));
    assert_eq!(
        items[0]["plugin_ref"],
        json!(gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID)
    );
    assert_eq!(
        items[1]["config"]["required_request_headers"],
        json!("x-correlation-id")
    );
}

#[tokio::test]
async fn list_endpoints_honour_the_odata_parameters() {
    let fx = Fixture::start().await;
    for alias in ["alpha", "bravo", "charlie"] {
        let mut body = upstream_body(&fx.upstream.host(), fx.upstream.port(), Some(alias));
        if let Some(object) = body.as_object_mut() {
            object.insert("tags".to_owned(), json!([alias]));
        }
        assert_eq!(
            post_json(&fx.api_url("/upstreams"), &body).await.status,
            StatusCode::CREATED
        );
    }

    let all = get(&fx.api_url("/upstreams")).await.json();
    assert_eq!(all["count"], json!(3));

    let filtered = get(&fx.api_url("/upstreams?$filter=alias%20eq%20'bravo'"))
        .await
        .json();
    assert_eq!(filtered["count"], json!(1));
    assert_eq!(filtered["items"][0]["alias"], json!("bravo"));

    let ordered = get(&fx.api_url("/upstreams?$orderby=alias%20desc&$top=2"))
        .await
        .json();
    assert_eq!(ordered["count"], json!(2));
    assert_eq!(ordered["items"][0]["alias"], json!("charlie"));

    let paged = get(&fx.api_url("/upstreams?$orderby=alias&$skip=1&$top=1"))
        .await
        .json();
    assert_eq!(paged["items"][0]["alias"], json!("bravo"));

    let projected = get(&fx.api_url("/upstreams?$select=alias")).await.json();
    assert!(projected["items"][0].get("server").is_none());
    assert!(projected["items"][0].get("alias").is_some());

    let rejected = get(&fx.api_url("/upstreams?alias=bravo")).await;
    assert_gateway_problem(&rejected, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn a_malformed_resource_id_is_a_validation_error() {
    let fx = Fixture::start().await;
    let res = get(&fx.api_url("/upstreams/not-an-identifier")).await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);

    // A well-formed identifier of the wrong type is equally invalid.
    let wrong_type = format!("{}{}", gts_helpers::ROUTE_TYPE, uuid::Uuid::new_v4());
    let res = get(&fx.api_url(&format!("/upstreams/{wrong_type}"))).await;
    assert_gateway_problem(&res, StatusCode::BAD_REQUEST, errors::VALIDATION);
}

#[tokio::test]
async fn every_documented_management_path_is_reachable() {
    let fx = Fixture::start().await;
    let host = fx.upstream.host();
    let port = fx.upstream.port();
    let upstream_id = post_json(
        &fx.api_url("/upstreams"),
        &upstream_body(&host, port, Some("mock")),
    )
    .await
    .json()["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let route_id = post_json(
        &fx.api_url("/routes"),
        &json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        }),
    )
    .await
    .json()["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let plugin_id = post_json(
        &fx.api_url("/plugins"),
        &json!({
            "name": "guard",
            "plugin_type": "guard",
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        }),
    )
    .await
    .json()["uuid"]
        .as_str()
        .expect("uuid")
        .to_owned();

    let cases: Vec<(Method, String)> = vec![
        (Method::GET, "/upstreams".to_owned()),
        (Method::GET, format!("/upstreams/{upstream_id}")),
        (Method::GET, "/routes".to_owned()),
        (Method::GET, format!("/routes/{route_id}")),
        (Method::GET, "/plugins".to_owned()),
        (Method::GET, format!("/plugins/{plugin_id}")),
        (Method::GET, format!("/plugins/{plugin_id}/source")),
    ];
    for (method, path) in cases {
        let res = send(method.clone(), &fx.api_url(&path), &[], None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "{method} {path} should be served; body: {}",
            String::from_utf8_lossy(&res.body)
        );
    }
}
