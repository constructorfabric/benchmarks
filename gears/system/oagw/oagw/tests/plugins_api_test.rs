// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-rest-api:p2
//! Custom-plugin management API (ADR-0002): immutability (no PUT), the
//! `text/plain` source endpoint, name conflicts and the ADR-0001
//! `plugin.in_use` deletion guard.

mod common;

use anyhow::{Context, Result};
use axum::http::{StatusCode, header};
use uuid::Uuid;

use common::{
    AUTH_APIKEY, AUTH_PLUGIN_STEM, Harness, PROTOCOL_HTTP, TRANSFORM_PLUGIN_STEM, endpoint,
    https_upstream, plugin_payload,
};

fn tenant() -> Uuid {
    Uuid::now_v7()
}

const SOURCE: &str = "def transform(ctx, config):\n    ctx.request.headers['x-trace'] = 'on'\n";

async fn seed_plugin(harness: &Harness, owner: Uuid, name: &str) -> Result<Uuid> {
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            owner,
            Some(plugin_payload(name, SOURCE)),
        )
        .await?;
    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let id = reply.problem_field("id").context("id")?;
    Ok(Uuid::parse_str(id)?)
}

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

// ── Creation & shape ─────────────────────────────────────────────────────

#[tokio::test]
async fn create_plugin_returns_201_with_the_bare_uuid() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            owner,
            Some(plugin_payload("redact", SOURCE)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    let id = reply.problem_field("id").context("id")?;
    assert!(
        Uuid::parse_str(id).is_ok(),
        "expected a bare UUID, got {id}"
    );
    assert_eq!(reply.problem_field("plugin_type"), Some("transform"));
    assert_eq!(reply.problem_field("name"), Some("redact"));
    assert_eq!(reply.problem_field("source_code"), Some(SOURCE));
    assert_eq!(reply.problem_field("enabled"), None);
    Ok(())
}

#[tokio::test]
async fn create_plugin_requires_a_name_and_source() -> Result<()> {
    let harness = Harness::new();
    let missing_name = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(serde_json::json!({ "plugin_type": "transform", "source_code": SOURCE })),
        )
        .await?;
    assert_eq!(missing_name.status, StatusCode::BAD_REQUEST);

    let missing_source = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(serde_json::json!({ "name": "redact", "plugin_type": "transform" })),
        )
        .await?;
    assert_eq!(missing_source.status, StatusCode::BAD_REQUEST);

    let unknown_kind = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(serde_json::json!({ "name": "redact", "plugin_type": "widget", "source_code": SOURCE })),
        )
        .await?;
    assert_eq!(unknown_kind.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn an_empty_or_oversized_source_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let blank = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(plugin_payload("blank", "   \n")),
        )
        .await?;
    assert_eq!(
        blank.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        blank.text
    );

    let oversized = "x".repeat(256 * 1024 + 1);
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(plugin_payload("big", &oversized)),
        )
        .await?;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn an_oversized_plugin_config_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "name": "blobby",
        "plugin_type": "transform",
        "source_code": SOURCE,
        "config": { "blob": "x".repeat(16 * 1024 + 1) }
    });
    let reply = harness
        .call("POST", "/oagw/v1/plugins", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_plugin_name_is_409() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    seed_plugin(&harness, owner, "redact").await?;
    let second = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            owner,
            Some(plugin_payload("redact", SOURCE)),
        )
        .await?;

    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(
        second.problem_type(),
        Some(common::problem_type("plugin.conflict.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn the_same_plugin_name_is_fine_in_another_tenant() -> Result<()> {
    let harness = Harness::new();
    seed_plugin(&harness, tenant(), "redact").await?;
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/plugins",
            tenant(),
            Some(plugin_payload("redact", SOURCE)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn plugins_are_immutable_put_is_405() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let id = seed_plugin(&harness, owner, "redact").await?;

    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/plugins/{id}"),
            owner,
            Some(plugin_payload("renamed", SOURCE)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

// ── Read / source ────────────────────────────────────────────────────────

#[tokio::test]
async fn get_plugin_accepts_the_gts_form_of_the_id() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let id = seed_plugin(&harness, owner, "redact").await?;
    let gts_id = format!("{TRANSFORM_PLUGIN_STEM}{id}");

    let reply = harness
        .call("GET", &format!("/oagw/v1/plugins/{gts_id}"), owner, None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.problem_field("id"), Some(id.to_string().as_str()));
    Ok(())
}

#[tokio::test]
async fn source_endpoint_returns_text_plain() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let id = seed_plugin(&harness, owner, "redact").await?;

    let bare = harness
        .call("GET", &format!("/oagw/v1/plugins/{id}/source"), owner, None)
        .await?;
    assert_eq!(bare.status, StatusCode::OK);
    let content_type = bare
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .context("content-type")?;
    assert!(content_type.starts_with("text/plain"), "got {content_type}");
    assert_eq!(bare.text, SOURCE);

    let gts_id = format!("{TRANSFORM_PLUGIN_STEM}{id}");
    let gts = harness
        .call(
            "GET",
            &format!("/oagw/v1/plugins/{gts_id}/source"),
            owner,
            None,
        )
        .await?;
    assert_eq!(gts.status, StatusCode::OK);
    assert_eq!(gts.text, SOURCE);
    Ok(())
}

#[tokio::test]
async fn foreign_plugins_are_invisible() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let id = seed_plugin(&harness, owner, "redact").await?;
    let stranger = tenant();

    let reply = harness
        .call("GET", &format!("/oagw/v1/plugins/{id}"), stranger, None)
        .await?;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("plugin.not_found.v1"))
    );

    let source = harness
        .call(
            "GET",
            &format!("/oagw/v1/plugins/{id}/source"),
            stranger,
            None,
        )
        .await?;
    assert_eq!(source.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn plugin_lists_scopes_to_the_tenant() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    seed_plugin(&harness, owner, "redact").await?;
    seed_plugin(&harness, owner, "annotate").await?;

    let mine = harness.call("GET", "/oagw/v1/plugins", owner, None).await?;
    assert_eq!(
        mine.json
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(2)
    );
    // `$orderby=name asc`.
    let ordered = harness
        .call("GET", "/oagw/v1/plugins?$orderby=name%20asc", owner, None)
        .await?;
    let items = ordered
        .json
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let names: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(names, vec!["annotate", "redact"]);
    Ok(())
}

// ── Binding validation (DESIGN §3.2 resolution algorithm) ────────────────

#[tokio::test]
async fn binding_a_resolvable_built_in_plugin_is_accepted() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "sharing": "private", "items": [
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let items = reply
        .json
        .pointer("/plugins/items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1);
    Ok(())
}

#[tokio::test]
async fn binding_a_catalogued_but_unresolvable_plugin_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1"] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn binding_an_unknown_plugin_reference_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": ["not-a-plugin-reference"] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn binding_a_foreign_custom_plugin_reports_503() -> Result<()> {
    let harness = Harness::new();
    let plugin_id = seed_plugin(&harness, tenant(), "redact").await?;
    let reference = format!("{TRANSFORM_PLUGIN_STEM}{plugin_id}");
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [reference] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "body: {}",
        reply.text
    );
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("plugin.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_bare_uuid_plugin_reference_resolves() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let plugin_id = seed_plugin(&harness, owner, "redact").await?;
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [plugin_id.to_string()] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    Ok(())
}

#[tokio::test]
async fn a_bare_uuid_reference_to_an_unknown_plugin_reports_503() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [Uuid::new_v4().to_string()] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_configured_binding_is_accepted_and_round_trips_verbatim() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
            {
                "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                "config": {
                    "required_request_headers": " x-correlation-id , X-Tenant-Id ",
                    "required_response_headers": "x-signature"
                }
            }
        ] }
    });
    // The configured binding keeps every member the caller sent, so a body that
    // was parsed and re-serialised is byte-identical to the one sent.
    let sent = serde_json::to_string(&payload.pointer("/plugins/items"))
        .context("serialising the request items")?;
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let stored = serde_json::to_string(&reply.json.pointer("/plugins/items"))
        .context("serialising the stored items")?;
    assert_eq!(stored, sent);
    Ok(())
}

#[tokio::test]
async fn a_configured_binding_is_accepted_on_a_route() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let mut payload = common::route_payload(upstream_id, &["GET"], "/v1/chat");
    payload["plugins"] = serde_json::json!({ "items": [{
        "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        "config": { "required_request_headers": "x-correlation-id" }
    }] });
    let reply = harness
        .call("POST", "/oagw/v1/routes", owner, Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let item = reply
        .json
        .pointer("/plugins/items/0")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        item.get("plugin_ref").and_then(serde_json::Value::as_str),
        Some("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
    );
    assert_eq!(
        item.pointer("/config/required_request_headers")
            .and_then(serde_json::Value::as_str),
        Some("x-correlation-id")
    );
    Ok(())
}

#[tokio::test]
async fn a_configured_binding_without_a_plugin_ref_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [{ "config": { "header": "x-trace" } }] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_configured_binding_of_an_unresolvable_plugin_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [{
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
            "config": { "required_request_headers": "x-correlation-id" }
        }] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_configured_binding_of_a_foreign_plugin_reports_503() -> Result<()> {
    let harness = Harness::new();
    let plugin_id = seed_plugin(&harness, tenant(), "redact").await?;
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [{
            "plugin_ref": format!("{TRANSFORM_PLUGIN_STEM}{plugin_id}"),
            "config": { "header": "x-trace" }
        }] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "body: {}",
        reply.text
    );
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("plugin.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn an_auth_plugin_in_the_plugin_chain_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [AUTH_APIKEY] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    assert!(
        reply.text.contains("auth"),
        "the rejection must name the family: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_bare_auth_plugin_name_in_the_plugin_chain_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": ["apikey"] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    assert!(
        reply.text.contains("'auth' binding"),
        "the rejection must point at the auth member: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn an_auth_plugin_of_a_custom_record_in_the_chain_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let payload = serde_json::json!({
        "name": "gateway-login",
        "plugin_type": "auth",
        "source_code": "def transform(ctx, config):\n    pass\n",
        "config": { "header": "x-trace" }
    });
    let created = harness
        .call("POST", "/oagw/v1/plugins", owner, Some(payload))
        .await?;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "body: {}",
        created.text
    );
    let id = Uuid::parse_str(created.problem_field("id").context("id")?)?;
    let binding = serde_json::json!({
        "plugin_ref": format!("{AUTH_PLUGIN_STEM}{id}"),
        "config": { "header": "x-trace" }
    });
    let upstream = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [binding] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(upstream))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn the_nested_auth_config_of_the_adr_shape_is_accepted() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "login.microsoftonline.com", 443)] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
            "config": {
                "token_endpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
                "client_id_ref": "cred://ms-graph-client-id",
                "client_secret_ref": "cred://ms-graph-client-secret",
                "scopes": "https://graph.microsoft.com/.default"
            }
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    let stored = reply.json.pointer("/auth");
    assert_eq!(
        stored
            .and_then(|auth| auth.get("type"))
            .and_then(serde_json::Value::as_str),
        Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1")
    );
    assert_eq!(
        stored
            .and_then(|auth| auth.pointer("/config/client_id_ref"))
            .and_then(serde_json::Value::as_str),
        Some("cred://ms-graph-client-id")
    );
    Ok(())
}

#[tokio::test]
async fn a_blank_auth_type_is_refused() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["auth"] = serde_json::json!({ "type": "  " });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_plugin_reference_of_the_wrong_family_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let plugin_id = seed_plugin(&harness, owner, "redact").await?;
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [common::gts_resource_id("auth_plugin", plugin_id)] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

// ── Deletion (ADR-0001 plugin deletion behaviour) ────────────────────────

#[tokio::test]
async fn unbound_plugin_deletes_with_204() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let id = seed_plugin(&harness, owner, "redact").await?;

    let deleted = harness
        .call("DELETE", &format!("/oagw/v1/plugins/{id}"), owner, None)
        .await?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(deleted.text.is_empty());

    let gone = harness
        .call("GET", &format!("/oagw/v1/plugins/{id}"), owner, None)
        .await?;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn plugin_bound_to_an_upstream_is_in_use() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let plugin_id = seed_plugin(&harness, owner, "redact").await?;
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [format!("{TRANSFORM_PLUGIN_STEM}{plugin_id}")] }
    });
    let upstream = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;
    assert_eq!(
        upstream.status,
        StatusCode::CREATED,
        "body: {}",
        upstream.text
    );

    let reply = harness
        .call(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            owner,
            None,
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CONFLICT, "body: {}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("plugin.in_use.v1"))
    );
    assert_eq!(
        reply
            .json
            .get("plugin_id")
            .and_then(serde_json::Value::as_str),
        Some(plugin_id.to_string().as_str())
    );
    let referenced = reply
        .json
        .pointer("/referenced_by/upstreams")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(referenced.len(), 1);
    // The record survives.
    let still_there = harness
        .call("GET", &format!("/oagw/v1/plugins/{plugin_id}"), owner, None)
        .await?;
    assert_eq!(still_there.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn plugin_bound_to_a_route_is_in_use_with_route_references() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let plugin_id = seed_plugin(&harness, owner, "redact").await?;
    let upstream_id = seed_upstream(&harness, owner, "api.openai.com").await?;
    let payload = serde_json::json!({
        "upstream_id": upstream_id.to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "plugins": { "items": [format!("{TRANSFORM_PLUGIN_STEM}{plugin_id}")] }
    });
    harness
        .call("POST", "/oagw/v1/routes", owner, Some(payload))
        .await?;

    let reply = harness
        .call(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            owner,
            None,
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("plugin.in_use.v1"))
    );
    assert_eq!(
        reply
            .json
            .pointer("/referenced_by/routes")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        reply
            .json
            .pointer("/referenced_by/upstreams")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    Ok(())
}

#[tokio::test]
async fn deleting_the_binding_frees_the_plugin() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let plugin_id = seed_plugin(&harness, owner, "redact").await?;
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "plugins": { "items": [format!("{TRANSFORM_PLUGIN_STEM}{plugin_id}")] }
    });
    let upstream = harness
        .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
        .await?;
    let upstream_id = upstream.problem_field("id").context("id")?;

    // Replacing the upstream without the plugin binding clears the reference.
    let replacement = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] }
    });
    harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            owner,
            Some(replacement),
        )
        .await?;

    let deleted = harness
        .call(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            owner,
            None,
        )
        .await?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn deleting_a_foreign_plugin_is_404() -> Result<()> {
    let harness = Harness::new();
    let id = seed_plugin(&harness, tenant(), "redact").await?;
    let reply = harness
        .call("DELETE", &format!("/oagw/v1/plugins/{id}"), tenant(), None)
        .await?;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    Ok(())
}
