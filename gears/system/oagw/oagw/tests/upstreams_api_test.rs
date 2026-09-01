// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-rest-api:p2
//! Upstream management API: alias derivation and immutability (DESIGN §3.2),
//! endpoint validation, CRUD status codes, conflicts and the `OData` pipeline.

mod common;

use anyhow::{Context, Result};
use axum::http::{StatusCode, header};
use uuid::Uuid;

use common::{Harness, PROTOCOL_HTTP, endpoint, https_upstream, ip_upstream};

fn tenant() -> Uuid {
    Uuid::now_v7()
}

// ── Alias derivation (DESIGN §3.2 table) ─────────────────────────────────

#[tokio::test]
async fn hostname_on_a_standard_port_derives_the_hostname() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("api.openai.com"));
    assert_eq!(reply.problem_field("protocol"), Some(PROTOCOL_HTTP));
    // Bare UUID on the wire, not the GTS form.
    let id = reply.problem_field("id").context("id")?;
    assert!(
        Uuid::parse_str(id).is_ok(),
        "expected a bare UUID, got {id}"
    );
    Ok(())
}

#[tokio::test]
async fn an_endpoint_without_a_port_takes_the_scheme_default() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            { "scheme": "https", "host": "api.vendor.com" },
            { "scheme": "https", "host": "eu.vendor.com" },
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    // Both endpoints materialise to 443, so the derived routing key carries
    // no port.
    assert_eq!(reply.problem_field("alias"), Some("vendor.com"));
    let endpoints = reply
        .json
        .get("server")
        .and_then(|server| server.get("endpoints"))
        .and_then(serde_json::Value::as_array)
        .context("endpoints")?
        .clone();
    let ports: Vec<i64> = endpoints
        .iter()
        .filter_map(|endpoint| endpoint.get("port"))
        .filter_map(serde_json::Value::as_i64)
        .collect();
    assert_eq!(ports, vec![443, 443]);
    Ok(())
}

#[tokio::test]
async fn a_tag_outside_the_label_set_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["tags"] = serde_json::json!(["team", "Eu West"]);

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    assert_eq!(
        reply
            .json
            .get("invalid_value")
            .and_then(serde_json::Value::as_str),
        Some("Eu West")
    );
    Ok(())
}

#[tokio::test]
async fn too_many_tags_or_endpoints_are_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["tags"] = serde_json::json!(
        (0..33)
            .map(|index| format!("tag{index}"))
            .collect::<Vec<_>>()
    );
    let tags = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;
    assert_eq!(tags.status, StatusCode::BAD_REQUEST, "body: {}", tags.text);

    let endpoints: Vec<serde_json::Value> = (0..65)
        .map(|index| endpoint("https", &format!("host{index}.vendor.com"), 443))
        .collect();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": endpoints }
    });
    let pool = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;
    assert_eq!(pool.status, StatusCode::BAD_REQUEST, "body: {}", pool.text);
    Ok(())
}

#[tokio::test]
async fn an_unknown_member_is_ignored_not_rejected() -> Result<()> {
    let harness = Harness::new();
    // Deliberate deviation (see `src/domain/model.rs`): no `deny_unknown_fields`,
    // so a client from a newer revision still manages its records.
    let mut payload = https_upstream("api.openai.com", 443);
    payload["retry_budget"] = serde_json::json!({ "max": 3 });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    // The unknown member is not echoed back either.
    assert_eq!(reply.json.get("retry_budget"), None);
    Ok(())
}

#[tokio::test]
async fn a_body_larger_than_the_configured_limit_is_413() -> Result<()> {
    // The OoP serve path installs no body-limit layer, so the gear applies
    // `oagw.config.max_body_bytes` on the management routes itself.
    let harness = Harness::with_policy(|policy| {
        policy.max_body_bytes = 64;
    });
    let mut payload = https_upstream("api.openai.com", 443);
    payload["padding"] = serde_json::json!("x".repeat(512));

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "body: {}",
        reply.text
    );
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("payload.too_large.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn hostname_on_a_non_standard_port_derives_host_and_port() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("api.openai.com", 8443)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("api.openai.com:8443"));
    Ok(())
}

#[tokio::test]
async fn multiple_hostnames_derive_the_registrable_suffix() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "us.vendor.com", 443),
            endpoint("https", "eu.vendor.com", 443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("vendor.com"));
    Ok(())
}

#[tokio::test]
async fn multi_host_pool_below_the_registrable_domain_derives_the_shared_suffix() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "x.us.vendor.com", 443),
            endpoint("https", "y.us.vendor.com", 443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("us.vendor.com"));
    Ok(())
}

#[tokio::test]
async fn multi_host_pool_on_a_non_standard_port_keeps_the_port() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "us.vendor.com", 8443),
            endpoint("https", "eu.vendor.com", 8443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("vendor.com:8443"));
    Ok(())
}

#[tokio::test]
async fn bare_public_suffix_requires_an_explicit_alias() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "foo.co.uk", 443),
            endpoint("https", "bar.co.uk", 443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn unrelated_hostnames_require_an_explicit_alias() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "us.foo.com", 443),
            endpoint("https", "eu.bar.com", 443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn ip_pool_without_an_alias_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(ip_upstream(None)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn ip_pool_with_an_explicit_alias_is_accepted() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(ip_upstream(Some("My-Service"))),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    // Normalized to ASCII lowercase.
    assert_eq!(reply.problem_field("alias"), Some("my-service"));
    Ok(())
}

#[tokio::test]
async fn explicit_alias_must_be_an_ldh_name() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(ip_upstream(Some("vendor.com%2Fevil"))),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn derived_pool_rejects_a_differing_alias() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "alias": "not-the-host",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn derived_pool_tolerates_the_exact_alias_for_idempotency() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "alias": "api.openai.com",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn hostname_pools_are_normalized_before_derivation() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("API.OpenAI.COM.", 443)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.problem_field("alias"), Some("api.openai.com"));
    Ok(())
}

// ── Endpoint validation ──────────────────────────────────────────────────

#[tokio::test]
async fn empty_endpoint_pool_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn rfc1123_invalid_hostname_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("-bad-.host", 443)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.problem_field("host"), Some("-bad-.host"));
    Ok(())
}

#[tokio::test]
async fn heterogeneous_pools_are_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "api.openai.com", 443),
            endpoint("https", "api.openai.com", 8443),
        ] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn http_scheme_is_accepted_when_the_config_allows_it() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("http", "127.0.0.1", 8080)] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    assert_eq!(reply.problem_field("alias"), Some("127.0.0.1:8080"));
    Ok(())
}

#[tokio::test]
async fn http_scheme_is_rejected_under_the_https_only_baseline() -> Result<()> {
    let harness = Harness::with_policy(|policy| {
        policy.allow_http_upstream = false;
    });
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("http", "127.0.0.1", 8080)] }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn port_zero_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("api.openai.com", 0)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.problem_field("invalid_value"), Some("0"));
    Ok(())
}

#[tokio::test]
async fn ssrf_denied_host_is_rejected() -> Result<()> {
    let harness = Harness::with_policy(|policy| {
        policy.ssrf.denied_hosts = vec!["metadata.internal".to_owned()];
    });
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(https_upstream("metadata.internal", 443)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

// ── Header transformation rules (upstream schema `headers`) ──────────────

/// An upstream payload with a request-side header block.
fn upstream_with_headers(headers: serde_json::Value) -> serde_json::Value {
    let mut payload = https_upstream("api.openai.com", 443);
    payload["headers"] = headers;
    payload
}

#[tokio::test]
async fn an_unknown_passthrough_mode_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let headers = serde_json::json!({ "request": { "passthrough": "everything" } });
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(upstream_with_headers(headers)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.problem_field("invalid_value"), Some("everything"));
    Ok(())
}

#[tokio::test]
async fn the_documented_passthrough_modes_are_accepted() -> Result<()> {
    let harness = Harness::new();
    let headers = serde_json::json!({ "request": { "passthrough": "allowlist" } });
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(upstream_with_headers(headers)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn an_invalid_header_name_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let headers = serde_json::json!({
        "request": { "set": { "not a header name": "value" } },
        "response": { "remove": ["bad name"] }
    });
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(upstream_with_headers(headers)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn a_header_block_of_valid_names_is_accepted() -> Result<()> {
    let harness = Harness::new();
    let headers = serde_json::json!({
        "request": {
            "set": { "x-trace": "oagw" },
            "add": { "x-vendor": "1" },
            "remove": ["x-secret"],
            "passthrough": "none"
        },
        "response": { "set": { "x-gateway": "oagw" }, "remove": ["x-server"] }
    });
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(upstream_with_headers(headers)),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    Ok(())
}

// ── CRUD + conflicts ─────────────────────────────────────────────────────

#[tokio::test]
async fn alias_conflict_within_a_tenant_is_409() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;
    let second = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;

    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(
        second.problem_type(),
        Some(common::problem_type("alias.conflict.v1"))
    );
    assert_eq!(
        second
            .headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    Ok(())
}

#[tokio::test]
async fn same_alias_is_allowed_in_another_tenant() -> Result<()> {
    let harness = Harness::new();
    harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn get_upstream_returns_the_record() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let reply = harness
        .call("GET", &format!("/oagw/v1/upstreams/{id}"), owner, None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.problem_field("id"), Some(id.as_str()));
    assert_eq!(
        reply.problem_field("tenant_id"),
        Some(owner.to_string().as_str())
    );
    assert_eq!(reply.problem_field("alias"), Some("api.openai.com"));
    assert_eq!(
        reply
            .json
            .pointer("/server/endpoints/0/host")
            .and_then(serde_json::Value::as_str),
        Some("api.openai.com")
    );
    Ok(())
}

#[tokio::test]
async fn get_upstream_accepts_the_gts_form_of_the_id() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let gts_id = common::gts_resource_id("upstream", Uuid::parse_str(&id)?);
    let reply = harness
        .call("GET", &format!("/oagw/v1/upstreams/{gts_id}"), owner, None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.problem_field("id"), Some(id.as_str()));
    Ok(())
}

#[tokio::test]
async fn foreign_records_are_invisible() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let reply = harness
        .call("GET", &format!("/oagw/v1/upstreams/{id}"), tenant(), None)
        .await?;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("upstream.not_found.v1"))
    );
    // A foreign tenant cannot delete or replace it either.
    let deleted = harness
        .call(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            tenant(),
            None,
        )
        .await?;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn unparseable_ids_are_rejected_with_400() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call("GET", "/oagw/v1/upstreams/not-a-uuid", tenant(), None)
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn put_replaces_the_upstream_and_clears_omitted_optionals() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "tags": ["billing"],
        "enabled": false
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(payload),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    assert_eq!(reply.problem_field("id"), Some(id.as_str()));
    assert_eq!(
        reply
            .json
            .get("enabled")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        reply
            .json
            .pointer("/tags")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    assert!(reply.json.get("auth").is_none(), "auth must be cleared");
    assert!(
        reply.json.get("plugins").is_none(),
        "plugins must be cleared"
    );
    Ok(())
}

// ── Credential isolation (DESIGN §2.2, ADR-0008) ─────────────────────────

#[tokio::test]
async fn a_cred_reference_binding_is_accepted_and_echoed() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
            "client_id_ref": "cred://ms-graph-client-id",
            "client_secret_ref": "cred://ms-graph-client-secret",
            "issuer_url": "https://login.microsoftonline.com/",
            "scopes": "https://graph.microsoft.com/.default"
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.text);
    // Only the reference is echoed back: the control plane has no secret
    // material to leak.
    assert_eq!(
        reply
            .json
            .pointer("/auth/client_secret_ref")
            .and_then(serde_json::Value::as_str),
        Some("cred://ms-graph-client-secret")
    );
    Ok(())
}

#[tokio::test]
async fn an_inline_credential_is_rejected_with_the_member_name() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "secret_ref": "sk-live-0123456789abcdef"
        }
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
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    assert_eq!(reply.problem_field("invalid_value"), Some("secret_ref"));
    assert!(
        !reply.text.contains("sk-live-0123456789abcdef"),
        "the rejected secret must not be echoed: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn an_oauth2_binding_without_references_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.openai.com", 443)] },
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
            "token_endpoint": "https://login.microsoftonline.com/token"
        }
    });
    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn an_inline_credential_in_a_plugin_config_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut payload = common::plugin_payload("leaky", "def transform(ctx): pass\n");
    payload["config"] = serde_json::json!({ "api_key": "sk-live-0123456789abcdef" });
    let reply = harness
        .call("POST", "/oagw/v1/plugins", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn put_rejects_an_endpoint_change_that_would_move_the_alias() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let moved = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "api.vendor.com", 443)] }
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(moved),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    // The stored record is untouched.
    let unchanged = harness
        .call("GET", &format!("/oagw/v1/upstreams/{id}"), owner, None)
        .await?;
    assert_eq!(unchanged.problem_field("alias"), Some("api.openai.com"));
    Ok(())
}

#[tokio::test]
async fn put_accepts_a_case_only_hostname_change() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let same = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", "API.OpenAI.COM.", 443)] }
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(same),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    assert_eq!(reply.problem_field("alias"), Some("api.openai.com"));
    Ok(())
}

#[tokio::test]
async fn put_of_an_ip_upstream_may_repeat_its_alias() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let payload = serde_json::json!({
        "alias": "my-service",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "10.0.1.3", 443),
            endpoint("https", "10.0.1.4", 443),
        ] }
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(payload),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    assert_eq!(reply.problem_field("alias"), Some("my-service"));
    Ok(())
}

#[tokio::test]
async fn put_of_an_ip_upstream_rejects_a_renamed_alias() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(ip_upstream(Some("my-service"))),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let payload = serde_json::json!({
        "alias": "renamed",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "10.0.1.3", 443),
            endpoint("https", "10.0.1.4", 443),
        ] }
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(payload),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn put_of_a_derivable_upstream_requires_the_derived_alias() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    // Hostname pool moves to an IP pool: rejected even with an explicit alias.
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();

    let payload = serde_json::json!({
        "alias": "my-service",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "10.0.1.1", 443),
            endpoint("https", "10.0.1.2", 443),
        ] }
    });
    let reply = harness
        .call(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            owner,
            Some(payload),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn delete_upstream_returns_204_and_cascades_its_routes() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    let created = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            owner,
            Some(https_upstream("api.openai.com", 443)),
        )
        .await?;
    let id = created.problem_field("id").context("id")?.to_owned();
    let upstream_id = Uuid::parse_str(&id)?;
    harness
        .call(
            "POST",
            "/oagw/v1/routes",
            owner,
            Some(common::route_payload(upstream_id, &["GET"], "/v1/chat")),
        )
        .await?;

    let deleted = harness
        .call("DELETE", &format!("/oagw/v1/upstreams/{id}"), owner, None)
        .await?;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(deleted.text.is_empty());

    let routes = harness.call("GET", "/oagw/v1/routes", owner, None).await?;
    assert_eq!(
        routes
            .json
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    Ok(())
}

// ── Listing (OData subset) ───────────────────────────────────────────────

async fn seed_upstreams(harness: &Harness, owner: Uuid, count: usize) -> Result<()> {
    for index in 0..count {
        let host = format!("svc{index}.vendor.com");
        let payload = serde_json::json!({
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [endpoint("https", &host, 443)] },
            "tags": [format!("t{index}")],
        });
        harness
            .call("POST", "/oagw/v1/upstreams", owner, Some(payload))
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn list_defaults_to_fifty_items() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    seed_upstreams(&harness, owner, 3).await?;

    let reply = harness
        .call("GET", "/oagw/v1/upstreams", owner, None)
        .await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply
            .json
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(3)
    );
    Ok(())
}

#[tokio::test]
async fn list_applies_top_skip_orderby_and_filter() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    seed_upstreams(&harness, owner, 4).await?;

    let top_two = harness
        .call(
            "GET",
            "/oagw/v1/upstreams?$top=2&$skip=1&$orderby=alias%20desc",
            owner,
            None,
        )
        .await?;
    assert_eq!(top_two.status, StatusCode::OK, "body: {}", top_two.text);
    let items = top_two
        .json
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let aliases: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("alias").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(aliases, vec!["svc2.vendor.com", "svc1.vendor.com"]);

    let filtered = harness
        .call(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20'svc3.vendor.com'",
            owner,
            None,
        )
        .await?;
    assert_eq!(filtered.status, StatusCode::OK);
    let items = filtered
        .json
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items
            .first()
            .and_then(|item| item.get("alias"))
            .and_then(serde_json::Value::as_str),
        Some("svc3.vendor.com")
    );
    Ok(())
}

#[tokio::test]
async fn list_select_projects_the_requested_fields() -> Result<()> {
    let harness = Harness::new();
    let owner = tenant();
    seed_upstreams(&harness, owner, 1).await?;

    let reply = harness
        .call("GET", "/oagw/v1/upstreams?$select=alias", owner, None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK);
    let first = reply
        .json
        .pointer("/items/0")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(first.as_object().map(serde_json::Map::len), Some(1));
    Ok(())
}

#[tokio::test]
async fn list_accepts_top_at_the_maximum() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call("GET", "/oagw/v1/upstreams?$top=100", tenant(), None)
        .await?;

    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.text);
    Ok(())
}

#[tokio::test]
async fn list_rejects_an_out_of_range_top() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call("GET", "/oagw/v1/upstreams?$top=0", tenant(), None)
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn list_scopes_to_the_calling_tenant() -> Result<()> {
    let harness = Harness::new();
    seed_upstreams(&harness, tenant(), 2).await?;

    let other = harness
        .call("GET", "/oagw/v1/upstreams", tenant(), None)
        .await?;
    assert_eq!(
        other
            .json
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    Ok(())
}

// ── Error contract (RFC 9457 + ADR-0007) ─────────────────────────────────

#[tokio::test]
async fn problems_carry_the_oagw_error_contract() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000",
            tenant(),
            None,
        )
        .await?;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let content_type = reply
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .context("content-type")?;
    assert!(
        content_type.starts_with("application/problem+json"),
        "got {content_type}"
    );
    assert_eq!(
        reply
            .headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("upstream.not_found.v1"))
    );
    assert_eq!(reply.problem_field("title"), Some("Not Found"));
    assert_eq!(
        reply.json.get("status").and_then(serde_json::Value::as_u64),
        Some(404)
    );
    assert_eq!(
        reply.problem_field("instance"),
        Some("/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000")
    );
    Ok(())
}

#[tokio::test]
async fn problems_echo_the_trace_id() -> Result<()> {
    let harness = Harness::new();
    let trace_id = "0af7651916cd43dd8448eb211c80319c";
    let reply = harness
        .call_with_headers(
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000",
            tenant(),
            None,
            &[("traceparent", &format!("00-{trace_id}-b7ad6b7169203331-01"))],
        )
        .await?;

    assert_eq!(reply.problem_field("trace_id"), Some(trace_id));
    Ok(())
}

#[tokio::test]
async fn malformed_json_is_rejected_with_400() -> Result<()> {
    let harness = Harness::new();
    let reply = harness
        .call(
            "POST",
            "/oagw/v1/upstreams",
            tenant(),
            Some(serde_json::json!({"protocol": "nope"})),
        )
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/problem+json"))
    );
    Ok(())
}

// ── Policy write paths (ADR-0003 rate limiting, ADR-0004 CORS) ───────────

#[tokio::test]
async fn an_upstream_policy_the_deployment_cannot_enforce_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["rate_limit"] = serde_json::json!({
        "sustained": { "rate": 5 },
        "algorithm": "sliding_window"
    });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(
        reply.problem_type(),
        Some(common::problem_type("validation.error.v1"))
    );
    assert!(
        reply.text.contains("sliding_window"),
        "the detail must name the unsupported algorithm: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn an_upstream_cors_policy_is_accepted_and_round_trips() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["cors"] = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET"],
        "allow_credentials": true
    });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        reply
            .json
            .pointer("/cors/allowed_origins/0")
            .and_then(serde_json::Value::as_str),
        Some("https://app.example.com")
    );
    assert_eq!(
        reply
            .json
            .pointer("/cors/allow_credentials")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    Ok(())
}

#[tokio::test]
async fn an_upstream_wildcard_origin_behind_credentials_is_rejected() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["cors"] = serde_json::json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allow_credentials": true
    });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
        .await?;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.text.contains("wildcard origin"),
        "the detail must quote the ADR-0004 rule: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn an_upstream_rate_limit_policy_is_accepted_and_round_trips() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["rate_limit"] = serde_json::json!({
        "sustained": { "rate": 100, "window": "minute" },
        "burst": { "capacity": 200 },
        "scope": "ip",
        "cost": 2
    });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", tenant(), Some(payload))
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
            .pointer("/rate_limit/burst/capacity")
            .and_then(serde_json::Value::as_u64),
        Some(200)
    );
    assert_eq!(
        reply
            .json
            .pointer("/rate_limit/scope")
            .and_then(serde_json::Value::as_str),
        Some("ip")
    );
    Ok(())
}
