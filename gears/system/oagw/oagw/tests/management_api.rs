//! Router-level tests for the management (control-plane) API.
//!
//! Every test drives the gear's own `Router`, built exactly as the server
//! builds it, so the status codes, bodies and error ids asserted here are the
//! ones a client of the management API observes on the wire.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{Harness, JsonConfig, PROTOCOL_HTTP, record, request};
use uuid::Uuid;

/// A working upstream document, with a placeholder endpoint.
fn upstream_document(alias: &str, port: u16) -> serde_json::Value {
    serde_json::json!({
        "alias": alias,
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": port }] },
    })
}

async fn harness() -> Harness {
    Harness::build(&JsonConfig::new(true, 1024 * 1024), Vec::new()).await.0
}

#[tokio::test]
async fn a_created_upstream_is_listed_and_readable() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("readable", 9000)),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "create returns 201");
    assert!(response.body["id"].as_str().is_some(), "an id is minted");
    assert_eq!(response.body["alias"], "readable");
    assert_eq!(response.body["protocol"], PROTOCOL_HTTP);
    assert_eq!(response.body["enabled"], true);
    assert_eq!(response.body["server"]["endpoints"][0]["scheme"], "http");

    let id = response.body["id"].as_str().unwrap().to_owned();
    let response = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/upstreams/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["alias"], "readable");

    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/upstreams", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["context"]["page"]["count"], 1, "the page counts its items");
    assert!(
        response.body["data"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["alias"] == "readable")),
        "the new upstream appears in the list"
    );
}

#[tokio::test]
async fn an_alias_is_derived_from_the_host_when_not_supplied() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    assert_eq!(response.body["alias"], "api.openai.com");
}

#[tokio::test]
async fn a_duplicate_alias_is_rejected() {
    let harness = harness().await;
    let first = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("duplicate", 9001)),
            ))
            .await,
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED);
    let second = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("duplicate", 9002)),
            ))
            .await,
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "a second alias in the same tenant is 409");
}

#[tokio::test]
async fn an_empty_endpoint_pool_is_rejected() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "no-endpoints",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [] },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        response.detail().to_lowercase().contains("endpoint"),
        "the problem names the failing field: {}",
        response.detail()
    );
}

#[tokio::test]
async fn an_unknown_endpoint_scheme_is_rejected() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "odd-scheme",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "gopher", "host": "example.test" }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "the scheme is not one of the five"
    );
}

#[tokio::test]
async fn http_scheme_is_accepted_by_the_management_api() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("plaintext", 9003)),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "`http` is a legal endpoint scheme; only dialling it is gated"
    );
}

#[tokio::test]
async fn an_alias_that_is_not_addressable_in_a_path_is_rejected() {
    let harness = harness().await;
    for alias in ["/leading-slash", "trailing/", "sp ace"] {
        let response = record(
            harness
                .serve(request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document(alias, 9004)),
                ))
                .await,
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "alias `{alias}` is not addressable in a path"
        );
    }
}

#[tokio::test]
async fn a_put_replaces_the_upstream_without_changing_its_alias() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("replaceable", 9005)),
            ))
            .await,
    )
    .await;
    let id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/upstreams/{id}"),
                Some(serde_json::json!({
                    "enabled": false,
                    "tags": ["after"],
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.body["alias"], "replaceable", "the alias survives a replacement");
    assert_eq!(response.body["enabled"], false);
    assert_eq!(response.body["tags"][0], "after");
}

#[tokio::test]
async fn an_endpoint_change_that_keeps_the_alias_is_allowed() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/upstreams/{id}"),
                Some(serde_json::json!({
                    "tags": ["same-alias"],
                    "server": {
                        "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.raw);
    assert_eq!(response.body["alias"], "api.openai.com");
}

#[tokio::test]
async fn an_endpoint_change_that_would_change_the_alias_is_rejected() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    let id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/upstreams/{id}"),
                Some(serde_json::json!({
                    "server": {
                        "endpoints": [{ "scheme": "https", "host": "api.another.com", "port": 443 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "the alias is the routing key; the operator deletes and re-creates instead"
    );
}

#[tokio::test]
async fn a_deleted_upstream_is_gone() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("deletable", 9006)),
            ))
            .await,
    )
    .await;
    let id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request("DELETE", &format!("/oagw/v1/upstreams/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let response = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/upstreams/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_upstream_id_is_a_404() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "GET",
                "/oagw/v1/upstreams/00000000-0000-0000-0000-00000000000a",
                None,
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        response.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn a_route_is_created_read_updated_and_deleted() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("routed", 9007)),
            ))
            .await,
    )
    .await;
    let upstream_id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": {
                        "http": { "methods": ["GET"], "path": "/v1", "path_suffix_mode": "append" }
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let route_id = response.body["id"].as_str().unwrap().to_owned();
    assert_eq!(response.body["upstream_id"], upstream_id.as_str());
    assert_eq!(response.body["match"]["http"]["methods"][0], "GET");

    let response = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/routes/{route_id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);

    let response = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/routes/{route_id}"),
                Some(serde_json::json!({ "enabled": false })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["enabled"], false);

    let response = record(
        harness
            .serve(request("DELETE", &format!("/oagw/v1/routes/{route_id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let response = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/routes/{route_id}"), None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_route_with_neither_http_nor_grpc_match_is_rejected() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("matchless", 9008)),
            ))
            .await,
    )
    .await;
    let upstream_id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({ "upstream_id": upstream_id, "match": {} })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_route_with_both_http_and_grpc_match_is_rejected() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("ambiguous", 9009)),
            ))
            .await,
    )
    .await;
    let upstream_id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": {
                        "http": { "methods": ["GET"] },
                        "grpc": { "service": "svc", "method": "rpc" },
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_route_for_an_unknown_upstream_is_rejected() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": "00000000-0000-0000-0000-00000000000b",
                    "match": { "http": { "methods": ["GET"] } },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "the upstream must exist");
}

#[tokio::test]
async fn a_route_with_an_empty_method_list_is_rejected() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("methodless", 9010)),
            ))
            .await,
    )
    .await;
    let upstream_id = created.body["id"].as_str().unwrap().to_owned();

    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": [] } },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_list_endpoint_supports_a_filter() {
    let harness = harness().await;
    for alias in ["filter-one", "filter-two"] {
        let response = record(
            harness
                .serve(request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document(alias, 9011)),
                ))
                .await,
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
    }

    let response = record(
        harness
            .serve(request(
                "GET",
                "/oagw/v1/upstreams?$filter=alias%20eq%20'filter-one'",
                None,
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let items = response.body["data"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "only the matching upstream comes back");
    assert_eq!(items[0]["alias"], "filter-one");
}

#[tokio::test]
async fn the_list_endpoint_supports_top_and_orderby() {
    let harness = harness().await;
    for alias in ["alpha", "beta", "gamma"] {
        let response = record(
            harness
                .serve(request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document(alias, 9015)),
                ))
                .await,
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
    }

    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/upstreams?$top=2&$orderby=alias%20desc", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let aliases: Vec<&str> = response.body["data"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item["alias"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(aliases.len(), 2, "`$top` limits the page");
    assert_eq!(aliases, vec!["gamma", "beta"], "`$orderby alias desc` sorts descending");

    let response = record(
        harness
            .serve(request("GET", "/oagw/v1/upstreams?$select=alias", None))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let first = response.body["data"][0].clone();
    assert_eq!(first.as_object().map(serde_json::Map::len), Some(1), "`$select` projects");
}

#[tokio::test]
async fn a_plugin_chain_round_trips_through_the_management_api() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "plugged",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9012 }]
                    },
                    "plugins": {
                        "items": [
                            common::REQUEST_ID_PLUGIN,
                            {
                                "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                                "config": { "required_request_headers": "x-correlation-id" },
                            },
                        ]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let items = response.body["plugins"]["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2, "both plugin references survive the round trip");
    assert_eq!(items[0], common::REQUEST_ID_PLUGIN);
    assert_eq!(items[1]["plugin_ref"], common::REQUIRED_HEADERS_PLUGIN);
    assert_eq!(
        items[1]["config"]["required_request_headers"],
        "x-correlation-id",
        "the configuration document is persisted"
    );
}

#[tokio::test]
async fn a_core_plugin_reference_cannot_be_bound_through_plugins_items() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "catalog-plugged",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9013 }]
                    },
                    "plugins": {
                        "items": [
                            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_token_exchange.v1"
                        ]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "core data-plane logic cannot be bound through plugins.items"
    );
}

#[tokio::test]
async fn credentials_with_a_wildcard_origin_are_rejected_at_validation_time() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "cred-wildcard",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9014 }]
                    },
                    "cors": {
                        "enabled": true,
                        "allowed_origins": ["*"],
                        "allow_credentials": true
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "allow_credentials with a wildcard origin is refused at validation time"
    );
}

#[tokio::test]
async fn an_unauthenticated_management_request_is_refused() {
    let harness = harness().await;
    let response = record(
        harness
            .serve_unauthenticated(request("GET", "/oagw/v1/upstreams", None))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the handlers extract a SecurityContext extension; without one the request fails"
    );
}

#[tokio::test]
async fn a_plugin_reference_naming_no_plugin_is_refused() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "dangling-plugin",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9014 }]
                    },
                    "plugins": {
                        "items": ["0b7f5a3e-6f5c-4b8e-9a2d-1c3e5f7a9b99"]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a UUID instance names a stored plugin, and there is none: {}",
        response.raw
    );
}

#[tokio::test]
async fn a_stored_custom_plugin_reference_binds() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/plugins",
                Some(serde_json::json!({
                    "plugin_type": "guard_plugin",
                    "name": "allow-list",
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);
    let id = created.body["id"].as_str().expect("plugin id").to_owned();

    let bound = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "custom-plugged",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9015 }]
                    },
                    "plugins": { "items": [bound] },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);
    let items = response.body["plugins"]["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["plugin_ref"], bound.as_str());
    assert_eq!(
        items[0]["plugin_uuid"].as_str(),
        Some(id.as_str()),
        "the UUID is carried beside the reference"
    );
}

#[tokio::test]
async fn a_named_plugin_binding_carries_no_uuid() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "named-plugged",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9016 }]
                    },
                    "plugins": { "items": [common::REQUIRED_HEADERS_PLUGIN] },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);
    let items = response.body["plugins"]["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items[0], common::REQUIRED_HEADERS_PLUGIN, "a bare reference stays bare");
}

#[tokio::test]
async fn a_route_keeps_its_upstream_through_a_replacement() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(upstream_document("routed", 9017)),
            ))
            .await,
    )
    .await;
    let upstream_id = created.body["id"].as_str().unwrap().to_owned();
    let route = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET"], "path": "/v1" } },
                })),
            ))
            .await,
    )
    .await;
    let route_id = route.body["id"].as_str().unwrap().to_owned();

    let replaced = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/routes/{route_id}"),
                Some(serde_json::json!({ "enabled": false })),
            ))
            .await,
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.raw);
    assert_eq!(
        replaced.body["upstream_id"], upstream_id.as_str(),
        "`upstream_id` is immutable and never part of a replacement body"
    );
}

#[tokio::test]
async fn a_cors_document_round_trips_through_the_management_api() {
    let harness = harness().await;
    let cors = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com", "https://alt.example.com"],
        "allowed_methods": ["GET", "POST"],
        "expose_headers": ["X-Request-ID"],
        "allow_credentials": true,
    });
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "cross-origin",
                    "protocol": PROTOCOL_HTTP,
                    "cors": cors,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9018 }]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);
    for (field, expected) in [
        ("enabled", serde_json::json!(true)),
        ("allowed_origins", cors["allowed_origins"].clone()),
        ("allowed_methods", cors["allowed_methods"].clone()),
        ("expose_headers", cors["expose_headers"].clone()),
        ("allow_credentials", serde_json::json!(true)),
    ] {
        assert_eq!(response.body["cors"][field], expected, "field `{field}`");
    }
}

#[tokio::test]
async fn a_heterogeneous_endpoint_pool_is_rejected() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "mixed-pool",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [
                            { "scheme": "http", "host": "127.0.0.1", "port": 9019 },
                            { "scheme": "http", "host": "127.0.0.1", "port": 9020 },
                        ]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "one pool, one port: {}",
        response.raw
    );
}

#[tokio::test]
async fn a_homogeneous_endpoint_pool_is_accepted() {
    let harness = harness().await;
    let response = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "same-pool",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [
                            { "scheme": "http", "host": "127.0.0.1", "port": 9021 },
                            { "scheme": "http", "host": "127.0.0.2", "port": 9021 },
                        ]
                    },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.raw);
}

#[tokio::test]
async fn a_plugin_can_be_created_read_and_deleted() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/plugins",
                Some(serde_json::json!({
                    "plugin_type": "transform_plugin",
                    "name": "annotator",
                    "source": "def transform(ctx): pass",
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);
    let id = created.body["id"].as_str().expect("plugin id").to_owned();

    let read = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/plugins/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.raw);
    assert_eq!(read.body["name"], "annotator");

    let source = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/plugins/{id}/source"), None))
            .await,
    )
    .await;
    assert_eq!(source.status, StatusCode::OK, "{}", source.raw);
    assert_eq!(source.body["source"], "def transform(ctx): pass");

    let deleted = record(
        harness
            .serve(request("DELETE", &format!("/oagw/v1/plugins/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "an unreferenced plugin goes without a fight: {}",
        deleted.raw
    );

    let gone = record(
        harness
            .serve(request("GET", &format!("/oagw/v1/plugins/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.raw);
}

#[tokio::test]
async fn a_plugin_in_use_cannot_be_deleted() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/plugins",
                Some(serde_json::json!({
                    "plugin_type": "guard_plugin",
                    "name": "referenced",
                })),
            ))
            .await,
    )
    .await;
    let id = created.body["id"].as_str().expect("plugin id").to_owned();
    let bound = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");
    let upstream = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": "in-use",
                    "protocol": PROTOCOL_HTTP,
                    "server": {
                        "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 9022 }]
                    },
                    "plugins": { "items": [bound] },
                })),
            ))
            .await,
    )
    .await;
    assert_eq!(upstream.status, StatusCode::CREATED, "{}", upstream.raw);

    let deleted = record(
        harness
            .serve(request("DELETE", &format!("/oagw/v1/plugins/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::CONFLICT,
        "a bound plugin cannot be deleted: {}",
        deleted.raw
    );
    assert_eq!(
        deleted.body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
}

#[tokio::test]
async fn a_plugin_has_no_replacement_operation() {
    let harness = harness().await;
    let created = record(
        harness
            .serve(request(
                "POST",
                "/oagw/v1/plugins",
                Some(serde_json::json!({ "plugin_type": "guard_plugin", "name": "fixed" })),
            ))
            .await,
    )
    .await;
    let id = created.body["id"].as_str().expect("plugin id").to_owned();
    let response = record(
        harness
            .serve(request(
                "PUT",
                &format!("/oagw/v1/plugins/{id}"),
                Some(serde_json::json!({ "name": "renamed" })),
            ))
            .await,
    )
    .await;
    assert_ne!(
        response.status,
        StatusCode::OK,
        "plugins are not replaceable: {}",
        response.raw
    );
}

#[tokio::test]
async fn a_descendant_cannot_read_or_delete_an_ancestor_upstream() {
    // A child below a parent that owns an upstream: the child's own tenant
    // scope is what every management read answers to.
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let harness = Harness::build_with_hierarchy(
        &JsonConfig::new(true, 1024 * 1024),
        Vec::new(),
        &[(child, parent)],
    )
    .await
    .0;
    let created = record(
        harness
            .serve_as(
                parent,
                request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document("inheritance", 9100)),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);
    let id = created.body["id"].as_str().expect("upstream id").to_owned();

    let read = record(
        harness
            .serve_as(child, request("GET", &format!("/oagw/v1/upstreams/{id}"), None))
            .await,
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "the ancestor's upstream is not in the descendant's scope: {}",
        read.raw
    );

    let deleted = record(
        harness
            .serve_as(
                child,
                request("DELETE", &format!("/oagw/v1/upstreams/{id}"), None),
            )
            .await,
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::NOT_FOUND,
        "nor can the descendant tear it down: {}",
        deleted.raw
    );

    let still_there = record(
        harness
            .serve_as(
                parent,
                request("GET", &format!("/oagw/v1/upstreams/{id}"), None),
            )
            .await,
    )
    .await;
    assert_eq!(
        still_there.status,
        StatusCode::OK,
        "the ancestor keeps what it owns: {}",
        still_there.raw
    );
}

#[tokio::test]
async fn a_descendant_sees_its_own_upstream_and_not_its_ancestors() {
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let harness = Harness::build_with_hierarchy(
        &JsonConfig::new(true, 1024 * 1024),
        Vec::new(),
        &[(child, parent)],
    )
    .await
    .0;
    let parent_upstream = record(
        harness
            .serve_as(
                parent,
                request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document("shared", 9200)),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(parent_upstream.status, StatusCode::CREATED);
    let child_upstream = record(
        harness
            .serve_as(
                child,
                request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document("shared", 9300)),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(
        child_upstream.status,
        StatusCode::CREATED,
        "an alias is unique per tenant, not per deployment: {}",
        child_upstream.raw
    );

    let listed = record(
        harness
            .serve_as(child, request("GET", "/oagw/v1/upstreams", None))
            .await,
    )
    .await;
    let aliases: Vec<&str> = listed.body["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["alias"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        aliases,
        vec!["shared"],
        "the listing is one tenant's, not the family's: {aliases:?}"
    );
}

#[tokio::test]
async fn an_alias_is_stored_normalized() {
    // Aliases are ASCII-lowercase with trailing dots stripped, and resolution
    // is case-insensitive, so what the management API hands back is the
    // normalized key, never what was typed.
    let harness = harness().await;
    let created = record(
        harness
            .serve(
                request(
                    "POST",
                    "/oagw/v1/upstreams",
                    Some(upstream_document("MixedCase.API.", 9400)),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.raw);
    assert_eq!(
        created.body["alias"], "mixedcase.api",
        "the alias is stored in its normalized form"
    );

    let read = record(
        harness
            .serve(request("GET", "/oagw/v1/proxy/mixedcase.api/echo", None))
            .await,
    )
    .await;
    assert!(
        read.detail().contains("no route matches the path"),
        "the alias resolved and only the route was missing: {}",
        read.detail()
    );
}
