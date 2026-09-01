// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-rest-api:p2
//! Route management API: CRUD status codes, match-rule uniqueness (409),
//! `upstream_id` immutability on PUT, ancestor invisibility and the cascade.

mod common;

use anyhow::{Context, Result};
use axum::http::StatusCode;
use uuid::Uuid;

use common::{Harness, PROTOCOL_HTTP, endpoint, http_match, https_upstream, route_payload};

fn tenant() -> Uuid {
    Uuid::now_v7()
}

/// Create an upstream and return its bare UUID.
async fn seed_upstream(harness: &Harness, owner: Uuid, host: &str) -> Result<Uuid> {
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream(host, 443)),
        )
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let id = reply.problem_field("id").context("id")?;
    Ok(Uuid::parse_str(id)?)
}

async fn seed_route(
    harness: &Harness,
    owner: Uuid,
    upstream_id: Uuid,
    path: &str,
) -> Result<serde_json::Value> {
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(route_payload(upstream_id, &["GET"], path)),
        )
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    Ok(reply.json)
}

#[tokio::test]
async fn create_route_returns_201_with_the_bare_uuid() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(route_payload(upstream_id, &["GET"], "/v1/chat")),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    let id = reply.problem_field("id").context("id")?;
    assert!(
        Uuid::parse_str(id).is_ok(),
        "expected a bare UUID, got {id}"
    );
    assert_eq!(
        reply.problem_field("upstream_id"),
        Some(upstream_id.to_string().as_str())
    );
    assert_eq!(
        reply
            .json
            .pointer("/match/http/methods/0")
            .and_then(serde_json::Value::as_str),
        Some("GET")
    );
    assert_eq!(
        reply
            .json
            .pointer("/match/http/path")
            .and_then(serde_json::Value::as_str),
        Some("/v1/chat")
    );
    Ok(())
}

#[tokio::test]
async fn create_route_accepts_a_grpc_match_rule() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "grpc.vendor.com").await?;
    let payload = serde_json::json!({
        "upstream_id": upstream_id.to_string(),
        "match": { "grpc": { "service": "cf.vendor.Echo", "method": "Ping" } }
    });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    assert_eq!(
        reply
            .json
            .pointer("/match/grpc/service")
            .and_then(serde_json::Value::as_str),
        Some("cf.vendor.Echo")
    );
    Ok(())
}

#[tokio::test]
async fn create_route_returns_404_for_a_foreign_upstream() -> Result<()> {
    let harness = Harness::new();
    let upstream_id = seed_upstream(&harness, tenant(), "api.openai.com").await?;
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            tenant(),
            Some(route_payload(upstream_id, &["GET"], "/v1")),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("upstream.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn create_route_returns_404_for_an_unknown_upstream() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            tenant(),
            Some(route_payload(Uuid::now_v7(), &["GET"], "/v1")),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn duplicate_match_rule_for_the_same_upstream_is_409() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    let second = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(route_payload(upstream_id, &["GET"], "/v1/chat")),
        )
        .await?;

    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(
        second.problem_type(),
        Some(common::problem_type("route.conflict.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn the_same_match_rule_is_fine_for_another_upstream() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let first = seed_upstream(&harness, owner, "api.openai.com").await?;
    let second = seed_upstream(&harness, owner, "api.vendor.com").await?;
    seed_route(&harness, owner, first, "/v1/chat").await?;
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(route_payload(second, &["GET"], "/v1/chat")),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    Ok(())
}

#[tokio::test]
async fn invalid_match_rules_are_rejected() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;

    // No branch at all.
    let empty = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(serde_json::json!({ "upstream_id": upstream_id.to_string(), "match": {} })),
        )
        .await?;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    // Both branches at once.
    let both = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(serde_json::json!({
                "upstream_id": upstream_id.to_string(),
                "match": {
                    "http": { "methods": ["GET"], "path": "/v1" },
                    "grpc": { "service": "svc", "method": "m" }
                }
            })),
        )
        .await?;
    assert_eq!(both.status, StatusCode::BAD_REQUEST);

    // Relative path.
    let relative = harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(serde_json::json!({
                "upstream_id": upstream_id.to_string(),
                "match": { "http": { "methods": ["GET"], "path": "v1" } }
            })),
        )
        .await?;
    assert_eq!(relative.status, StatusCode::BAD_REQUEST);
    assert_eq!(relative.problem_field("path"), Some("v1"));
    Ok(())
}

#[tokio::test]
async fn get_route_accepts_the_gts_form_of_the_id() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let created = seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    let id = created
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("id")?;

    let gts_id = common::gts_resource_id("route", Uuid::parse_str(id)?);
    let reply = harness
        .call("GET", &format!("/oagw/v1/routes/{gts_id}"), owner, None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.problem_field("id"), Some(id));
    Ok(())
}

#[tokio::test]
async fn foreign_routes_are_invisible() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let created = seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    let id = created
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("id")?;

    let stranger = tenant();
    let reply = harness
        .call("GET", &format!("/oagw/v1/routes/{id}"), stranger, None)
        .await?;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("route.not_found.v1"))
    );

    let deleted = harness
        .call("DELETE", &format!("/oagw/v1/routes/{id}"), stranger, None)
        .await?;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn put_replaces_the_route_but_keeps_the_upstream() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let first = seed_upstream(&harness, owner, "api.openai.com").await?;
    let second = seed_upstream(&harness, owner, "api.vendor.com").await?;
    let created = seed_route(&harness, owner, first, "/v1/chat").await?;
    let id = created
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("id")?;

    // `upstream_id` is not part of the update DTO (DESIGN §3.3 "PUT
    // (Replace)"), so a PUT cannot move a route to another upstream.
    let replacement = serde_json::json!({
        "upstream_id": second.to_string(),
        "match": http_match(&["POST"], "/v1/embeddings"),
        "tags": ["inference"],
    });
    let updated = harness
        .call(
            "PUT",
            &format!("/oagw/v1/routes/{id}"),
            owner,
            Some(replacement),
        )
        .await?;

    assert_eq!(updated.status, StatusCode::OK, "body: {}", updated.text);
    assert_eq!(updated.problem_field("id"), Some(id));
    assert_eq!(
        updated.problem_field("upstream_id"),
        Some(first.to_string().as_str())
    );
    assert_eq!(
        updated
            .json
            .pointer("/match/http/path")
            .and_then(serde_json::Value::as_str),
        Some("/v1/embeddings")
    );
    assert_eq!(
        updated
            .json
            .pointer("/tags")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[tokio::test]
async fn put_route_conflicts_are_reported_as_409() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    let other = seed_route(&harness, owner, upstream_id, "/v1/embeddings").await?;
    let id = other
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("id")?;

    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/routes/{id}"),
            owner,
            Some(serde_json::json!({ "match": http_match(&["GET"], "/v1/chat") })),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("route.conflict.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn delete_route_returns_204_and_an_empty_body() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let created = seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    let id = created
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("id")?;

    let deleted = harness
        .call("DELETE", &format!("/oagw/v1/routes/{id}"), owner, None)
        .await?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(deleted.text.is_empty());

    let missing = harness
        .call("GET", &format!("/oagw/v1/routes/{id}"), owner, None)
        .await?;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn routes_scoped_per_tenant() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    seed_route(&harness, owner, upstream_id, "/v1/chat").await?;

    let stranger = tenant();
    let empty = harness
        .call("GET", "/oagw/v1/routes", stranger, None)
        .await?;
    assert_eq!(
        empty
            .json
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    Ok(())
}

#[tokio::test]
async fn routes_support_the_odata_pipeline() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    seed_route(&harness, owner, upstream_id, "/v1/chat").await?;
    seed_route(&harness, owner, upstream_id, "/v1/embeddings").await?;

    let reply = harness
        .call(
            "GET",
            "/oagw/v1/routes?$filter=match%20eq%20'%2Fv1%2Fchat'",
            owner,
            None,
        )
        .await?;
    // `match` is not a filterable field of the documented subset, but the
    // pipeline must stay 200 and return something sane.
    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);

    let projected = harness
        .call("GET", "/oagw/v1/routes?$select=upstream_id", owner, None)
        .await?;
    assert_eq!(projected.status, StatusCode::OK, "body: {}", projected.text);
    let first = projected
        .json
        .pointer("/items/0")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(first.as_object().map(serde_json::Map::len), Some(1));
    Ok(())
}

#[tokio::test]
async fn upstream_cascade_removes_its_routes_only() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let first = seed_upstream(&harness, owner, "api.openai.com").await?;
    let second = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(serde_json::json!({
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [
                    endpoint("https", "api.vendor.com", 443),
                    endpoint("https", "eu.vendor.com", 443),
                ] }
            })),
        )
        .await?;
    let second_id = second.problem_field("id").context("id")?;
    let second_id = Uuid::parse_str(second_id)?;
    seed_route(&harness, owner, first, "/v1/chat").await?;
    seed_route(&harness, owner, second_id, "/v1/chat").await?;

    let deleted = harness
        .call(
            "DELETE",
            &format!("/oagw/v1/upstreams/{first}"),
            owner,
            None,
        )
        .await?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let remaining = harness.call("GET", "/oagw/v1/routes", owner, None).await?;
    let items = remaining
        .json
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items
            .first()
            .and_then(|route| route.get("upstream_id"))
            .and_then(serde_json::Value::as_str),
        Some(second_id.to_string().as_str())
    );
    Ok(())
}

#[tokio::test]
async fn a_route_policy_the_deployment_cannot_enforce_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;

    let body = route_payload(upstream_id, &["GET"], "/v1/chat");
    let body = serde_json::json!({
        "upstream_id": body["upstream_id"],
        "match": body["match"],
        "rate_limit": {
            "sustained": { "rate": 5 },
            "algorithm": "sliding_window"
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_route_cors_policy_is_accepted_and_round_trips() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;

    let body = route_payload(upstream_id, &["GET"], "/v1/chat");
    let body = serde_json::json!({
        "upstream_id": body["upstream_id"],
        "match": body["match"],
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"],
            "expose_headers": ["x-request-id"],
            "allow_credentials": true
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        reply
            .json
            .pointer("/cors/allow_credentials")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        reply
            .json
            .pointer("/cors/allowed_origins/0")
            .and_then(serde_json::Value::as_str),
        Some("https://app.example.com")
    );
    Ok(())
}

#[tokio::test]
async fn a_route_wildcard_origin_behind_credentials_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;

    let body = route_payload(upstream_id, &["GET"], "/v1/chat");
    let body = serde_json::json!({
        "upstream_id": body["upstream_id"],
        "match": body["match"],
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.text.contains("wildcard origin"),
        "the detail quotes the ADR rule: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_rate_limit_policy_is_accepted_and_round_trips() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;

    let body = route_payload(upstream_id, &["GET"], "/v1/chat");
    let body = serde_json::json!({
        "upstream_id": body["upstream_id"],
        "match": body["match"],
        "rate_limit": {
            "sustained": { "rate": 100, "window": "minute" },
            "burst": { "capacity": 10 },
            "scope": "user",
            "strategy": "queue",
            "cost": 2
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(body))
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        reply
            .json
            .pointer("/rate_limit/sustained/rate")
            .and_then(serde_json::Value::as_u64),
        Some(100)
    );
    assert_eq!(
        reply
            .json
            .pointer("/rate_limit/cost")
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );
    Ok(())
}
