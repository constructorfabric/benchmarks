//! Integration tests for the endpoint, sub-configuration and tag validation
//! outcomes over the HTTP surface
//! (`cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation`,
//! `cpt-cf-oagw-dod-upstream-management-nested-subconfig`,
//! `cpt-cf-oagw-dod-upstream-management-header-transform-config`,
//! `cpt-cf-oagw-dod-upstream-management-tags`,
//! `cpt-cf-oagw-dod-upstream-management-plugin-references`,
//! `cpt-cf-oagw-dod-upstream-management-auth-config`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::test_support::{
    FakePolicyAuthZ, FakeHierarchyTenantResolver, management_surface, security_context,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const APIKEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const REQUEST_ID_PLUGIN: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

fn endpoint(host: &str) -> Value {
    json!({ "scheme": "https", "host": host, "port": 443 })
}

fn pool(endpoints: &[Value]) -> Value {
    json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": endpoints } })
}

/// A pool mixing schemes, ports or protocols is rejected; a uniform two-endpoint
/// pool is accepted.
#[tokio::test]
async fn a_mixed_endpoint_pool_is_rejected_and_a_uniform_one_is_accepted() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        Some(json!({ "allow_http_upstream": true })),
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;

    for mixed in [
        pool(&[endpoint("a.vendor.com"), json!({ "scheme": "http", "host": "b.vendor.com", "port": 443 })]),
        pool(&[endpoint("a.vendor.com"), json!({ "scheme": "https", "host": "b.vendor.com", "port": 8443 })]),
        json!({
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [endpoint("a.vendor.com"), json!({ "scheme": "grpc", "host": "b.vendor.com", "port": 443 })]
            }
        }),
    ] {
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), mixed).await;
        assert_eq!(status, 400, "{bytes:?}");
    }

    let (status, bytes) = surface
        .create(tenant, Uuid::new_v4(), pool(&[endpoint("a.vendor.com"), endpoint("b.vendor.com")]))
        .await;
    assert_eq!(status, 201, "a uniform pool is accepted: {bytes:?}");
}

/// The scheme gate: `http` follows `allow_http_upstream`, and the other scheme
/// values are admitted with `https` applied when the scheme is omitted.
#[tokio::test]
async fn the_scheme_gate_follows_the_graded_configuration() {
    let tenant = Uuid::new_v4();
    let strict = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let permissive = management_surface(
        Some(json!({ "allow_http_upstream": true })),
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let http_pool = pool(&[json!({ "scheme": "http", "host": "api.vendor.com", "port": 80 })]);

    let (status, bytes) = strict.create(tenant, Uuid::new_v4(), http_pool.clone()).await;
    assert_eq!(status, 400, "{bytes:?}");
    let (status, _) = permissive.create(tenant, Uuid::new_v4(), http_pool).await;
    assert_eq!(status, 201, "`http` is admitted when the configuration allows it");

    for (scheme, host) in
        [("wss", "wss.vendor.com"), ("wt", "wt.vendor.com"), ("grpc", "grpc.vendor.com")]
    {
        let body = pool(&[json!({ "scheme": scheme, "host": host, "port": 443 })]);
        let (status, bytes) = strict.create(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 201, "`{scheme}` is a legal scheme: {bytes:?}");
    }

    // An omitted scheme defaults to `https`.
    let (status, bytes) = strict
        .create(tenant, Uuid::new_v4(), pool(&[json!({ "host": "defaulted.vendor.com" })]))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["server"]["endpoints"][0]["scheme"], "https");
}

/// A credential-bearing auth field must hold a `cred://` reference, and the
/// rejected value is never echoed.
#[tokio::test]
async fn a_non_cred_credential_value_is_rejected_without_an_echo() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let mut body = pool(&[endpoint("api.vendor.com")]);
    body["auth"] = json!({
        "type": APIKEY_PLUGIN,
        "config": { "api_key": "sk-live-0123456789abcdef" }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body).await;
    assert_eq!(status, 400, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    let wire = serde_json::to_string(&problem).expect("the problem renders");
    assert!(!wire.contains("sk-live-0123456789abcdef"), "no echo of the value: {wire}");
}

/// The header block accepts the documented rule keys and rejects an unknown key
/// and a `passthrough_allowlist` without `passthrough: allowlist`.
#[tokio::test]
async fn the_header_block_keeps_its_rule_keys() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let mut accepted = pool(&[endpoint("api.vendor.com")]);
    accepted["headers"] = json!({
        "request": {
            "set": { "x-forwarded-proto": "https" },
            "add": { "x-oagw-hop": "edge" },
            "remove": ["x-internal-token"],
            "passthrough": "allowlist",
            "passthrough_allowlist": ["authorization"]
        },
        "response": { "set": { "server": "oagw" } }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), accepted).await;
    assert_eq!(status, 201, "{bytes:?}");

    for headers in [
        json!({ "request": { "setx": { "a": "b" } } }),
        json!({ "request": { "passthrough_allowlist": ["authorization"] } }),
        json!({ "response": { "passthrough": "all" } }),
    ] {
        let mut body = pool(&[endpoint("api.vendor.com")]);
        body["headers"] = headers;
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{bytes:?}");
    }
}

/// A header rule whose name or value carries a control character, violates the
/// RFC 7230 grammar or exceeds 4096 bytes is a `400` naming the block and the
/// key, and stores nothing.
#[tokio::test]
async fn an_illegal_header_rule_is_rejected_naming_the_block_and_the_key() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    for (block, key, value) in [
        ("request", "x-bad\rname", "ok"),
        ("request", "x-bad\nname", "ok"),
        ("request", "x-bad\u{0}name", "ok"),
        ("response", "x-bad separator", "ok"),
        ("request", "x-ok", "bad\rvalue"),
        ("response", "x-ok", "bad\nvalue"),
        ("request", "x-long", Box::leak("v".repeat(4097).into_boxed_str())),
    ] {
        let mut body = pool(&[endpoint("api.vendor.com")]);
        body["headers"] = json!({ block: { "set": { key: value } } });
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{block}/{key}: {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem");
        let detail = problem["detail"].as_str().unwrap_or_default().to_owned();
        assert!(
            detail.contains(&format!("headers.{block}")),
            "the block is named: {problem}"
        );
        assert!(
            detail.contains(key.split(['\r', '\n', '\u{0}', ' ']).next().unwrap_or("x")),
            "the offending key is named: {problem}"
        );
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0, "an illegal rule stores nothing");
}

/// Tags are stored as rows on create, removed by an omission on replace and
/// preserved when the replacement carries them.
#[tokio::test]
async fn tag_rows_follow_the_write() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let mut created = pool(&[endpoint("api.vendor.com")]);
    created["tags"] = json!(["billing", "gold"]);
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), created).await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");
    assert_eq!(
        surface.gear.storage().expect("storage").row_counts()["oagw_upstream_tag"],
        2,
        "the tag rows are stored on create"
    );

    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(pool(&[endpoint("api.vendor.com")])),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    assert_eq!(
        surface.gear.storage().expect("storage").row_counts()["oagw_upstream_tag"],
        0,
        "an omitted tag removes its row"
    );

    let mut again = pool(&[endpoint("api.vendor.com")]);
    again["tags"] = json!(["billing"]);
    let (status, bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(again),
        )
        .await;
    assert_eq!(status, 200, "{bytes:?}");
    assert_eq!(
        surface.gear.storage().expect("storage").row_counts()["oagw_upstream_tag"],
        1,
        "an included tag preserves its row"
    );

    let mut illegal = pool(&[endpoint("api.vendor.com")]);
    illegal["tags"] = json!(["Not-Allowed"]);
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), illegal).await;
    assert_eq!(status, 400, "a tag outside `^[a-z0-9_-]+$` is rejected: {bytes:?}");
}

/// The nested `rate_limit` and `cors` blocks keep their schema shapes.
#[tokio::test]
async fn the_nested_rate_limit_and_cors_blocks_keep_their_shapes() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    for (name, block) in [
        ("rate_limit", json!({ "cost": 1 })),
        ("cors", json!({ "allowed_origins": ["*"] })),
        ("cors", json!({ "enabled": true, "allowed_origins": ["*"], "allow_credentials": true })),
        ("rate_limit", json!({ "sustained": { "rate": 1 }, "algorithm": "leaky_bucket" })),
        ("cors", json!({ "enabled": true, "allowed_methods": ["TRACE"] })),
    ] {
        let mut body = pool(&[endpoint("api.vendor.com")]);
        body[name] = block;
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{name}: {bytes:?}");
    }

    let mut accepted = pool(&[endpoint("api.vendor.com")]);
    accepted["rate_limit"] =
        json!({ "sharing": "private", "sustained": { "rate": 10, "window": "minute" }, "cost": 1 });
    accepted["cors"] = json!({
        "enabled": true,
        "sharing": "inherit",
        "allowed_origins": ["https://app.vendor.com"],
        "allowed_methods": ["GET", "POST"]
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), accepted).await;
    assert_eq!(status, 201, "{bytes:?}");
}

/// `plugins.items` holds builtin GTS identifiers and custom UUID references in
/// contiguous positions, and rejects an unknown reference or a duplicate.
#[tokio::test]
async fn the_plugin_chain_holds_its_positions_and_references() {
    let tenant = Uuid::new_v4();
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    // A custom reference binds only when the calling tenant holds the plugin
    // row it names, so the row is created first and its server-generated
    // identifier is the reference the binding carries.
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~",
                "name": "tenant-guard",
                "source_code": "const reference = 'opaque';"
            })),
        )
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let custom: Uuid = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("identifier")
        .parse()
        .expect("uuid");
    let mut accepted = pool(&[endpoint("api.vendor.com")]);
    accepted["plugins"] = json!({
        "sharing": "private",
        "items": [REQUEST_ID_PLUGIN, custom.to_string()]
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), accepted).await;
    assert_eq!(status, 201, "{bytes:?}");
    assert_eq!(
        surface.gear.storage().expect("storage").row_counts()["oagw_upstream_plugin"],
        2,
        "the ordered binding rows are stored"
    );

    for items in [
        json!(["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.not_a_plugin.v1"]),
        json!([REQUEST_ID_PLUGIN, REQUEST_ID_PLUGIN]),
        json!(["not even a reference"]),
    ] {
        let mut body = pool(&[endpoint("api.vendor.com")]);
        body["plugins"] = json!({ "sharing": "private", "items": items });
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), body).await;
        assert_eq!(status, 400, "{items}: {bytes:?}");
    }
}

/// An omitted sub-configuration block is never materialized, and an ancestor
/// record with an omitted block contributes nothing to a descendant's effective
/// configuration.
#[tokio::test]
async fn an_omitted_block_stays_absent_and_contributes_nothing() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let chain = [child, parent];
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&chain),
    )
    .await;
    let (status, bytes) = surface
        .create(parent, Uuid::new_v4(), pool(&[endpoint("api.vendor.com")]))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    for block in ["auth", "headers", "rate_limit", "cors", "plugins"] {
        assert!(
            record.get(block).is_none(),
            "no implicit `{block}` block was persisted: {record}"
        );
    }

    let (status, bytes) = surface
        .create(child, Uuid::new_v4(), pool(&[endpoint("api.vendor.com")]))
        .await;
    assert_eq!(status, 201, "the descendant create is accepted: {bytes:?}");
    let descendant: Value = serde_json::from_slice(&bytes).expect("record");
    for block in ["auth", "headers", "rate_limit", "cors", "plugins"] {
        assert!(
            descendant.get(block).is_none(),
            "an ancestor block that is absent contributes nothing: {descendant}"
        );
    }
}
