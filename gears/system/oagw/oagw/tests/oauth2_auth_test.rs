// Created: 2026-08-29 by Constructor Tech
//! OAuth2 client-credentials auth plugin: token fetch, caching and failures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Harness, json_body, post, tenant};
use httpmock::MockServer;
use serde_json::json;
use std::sync::Arc;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const OAUTH_FORM: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
const OAUTH_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
const CLIENT_ID_SECRET: &str = "b2F1dGgtY2xpZW50Om9hdXRoLXNlY3JldA=="; // oauth-client:oauth-secret

fn token_body(token: &str, expires_in: u64) -> String {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
}

/// Harness whose credential store knows the OAuth2 client credentials.
fn harness() -> Harness {
    let credstore = Arc::new(common::FakeCredStore::new(&[
        ("client-id", "oauth-client"),
        ("oauth-secret", "oauth-secret"),
    ]));
    Harness::new(common::test_config(), Some(credstore))
}

async fn register(
    harness: &Harness,
    alias: &str,
    server: &MockServer,
    auth: serde_json::Value,
) -> String {
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": alias,
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "auth": auth,
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

#[tokio::test]
async fn a_token_is_fetched_once_and_then_reused() {
    let server = MockServer::start();
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials")
            .body_includes("client_id=oauth-client");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_body("tok-1", 3600));
    });
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .header("authorization", "Bearer tok-1");
        then.status(200).body("ok");
    });

    let harness = harness();
    let alias = "oauth-cache.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
                "scopes": "read write",
            },
        }),
    )
    .await;

    for _ in 0..2 {
        let response = harness
            .send(
                "GET",
                &format!("/oagw/v1/proxy/{alias}/api"),
                None,
                tenant(),
            )
            .await;
        assert_eq!(response.status(), 200);
    }
    // The credential is minted once and replayed from the cache.
    assert_eq!(
        token.calls(),
        1,
        "the second request must be served from the cache"
    );
    assert_eq!(target.calls(), 2);
}

#[tokio::test]
async fn the_token_request_carries_the_client_credentials() {
    let server = MockServer::start();
    // `client_credentials` grant with the credentials in the form body.
    let form = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials")
            .body_includes("client_id=oauth-client")
            .body_includes("client_secret=oauth-secret")
            .body_includes("scope=read+write");
        then.status(200).body(token_body("tok-form", 3600));
    });
    // The `basic` variant carries them in the `Authorization` header instead.
    let basic = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .header("authorization", format!("Basic {CLIENT_ID_SECRET}"))
            .body_includes("grant_type=client_credentials");
        then.status(200).body(token_body("tok-basic", 3600));
    });
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = harness();
    let alias = "oauth-form.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
                "scopes": "read write",
            },
        }),
    )
    .await;
    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(form.calls(), 1);
    assert_eq!(basic.calls(), 0);

    let alias = "oauth-basic.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_BASIC,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;
    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(basic.calls(), 1, "the basic variant uses the header");
    assert!(target.calls() >= 2);
}

#[tokio::test]
async fn an_issuer_url_is_resolved_through_oidc_discovery() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/api");
        then.status(200).body("ok");
    });
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials");
        then.status(200).body(token_body("tok-discovered", 3600));
    });
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/.well-known/openid-configuration");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                json!({ "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()) })
                    .to_string(),
            );
    });

    let harness = harness();
    let alias = "oauth-issuer.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "issuer_url": format!("http://127.0.0.1:{}", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;

    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(target.calls(), 1);
    assert_eq!(
        token.calls(),
        1,
        "discovery must resolve the token endpoint"
    );
}

#[tokio::test]
async fn exactly_one_of_token_endpoint_or_issuer_url_is_accepted() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = harness();
    let alias = "oauth-both.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "issuer_url": format!("http://127.0.0.1:{}", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/oauth-both.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("OAUTH2_CONFIG_INVALID"),
        "the plugin error code must surface: {}",
        body["detail"]
    );

    let alias = "oauth-neither.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/oauth-neither.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        target.calls(),
        0,
        "a misconfigured plugin must not reach the upstream"
    );
}

#[tokio::test]
async fn a_rejected_token_fetch_is_never_cached() {
    let server = MockServer::start();
    let rejected = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials");
        then.status(400).body(r#"{"error":"invalid_client"}"#);
    });
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = harness();
    let alias = "oauth-reject.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;

    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    let source = common::header(&response, "x-oagw-error-source");
    assert_eq!(response.status(), 401);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(
        target.calls(),
        0,
        "no credential means the upstream is never reached"
    );
    assert_eq!(rejected.calls(), 1);

    // A second attempt reaches the IdP again: the failure is not cached.
    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 401);
    assert_eq!(rejected.calls(), 2);
}

#[tokio::test]
async fn a_short_lived_token_is_not_cached() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });
    // `expires_in: 10s` leaves no usable TTL once the 30s safety margin is
    // subtracted, so the token must not be cached.
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials");
        then.status(200).body(token_body("tok-short", 10));
    });

    let harness = harness();
    let alias = "oauth-short.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "oauth-secret",
            },
        }),
    )
    .await;

    for _ in 0..2 {
        let response = harness
            .send(
                "GET",
                &format!("/oagw/v1/proxy/{alias}/api"),
                None,
                tenant(),
            )
            .await;
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        token.calls(),
        2,
        "a token without a usable TTL is never cached"
    );
}

#[tokio::test]
async fn a_missing_client_secret_is_a_401_without_material() {
    let server = MockServer::start();
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST);
        then.status(200).body(token_body("tok-never", 3600));
    });

    // The credential store does not know `absent-secret`.
    let credstore = Arc::new(common::FakeCredStore::new(&[("client-id", "oauth-client")]));
    let harness = Harness::new(common::test_config(), Some(credstore));
    let alias = "oauth-nosecret.example.com";
    register(
        &harness,
        alias,
        &server,
        json!({
            "type": OAUTH_FORM,
            "config": {
                "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
                "client_id_ref": "cred://client-id",
                "client_secret_ref": "absent-secret",
            },
        }),
    )
    .await;

    let response = harness
        .send(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/api"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 401);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
    assert_eq!(
        token.calls(),
        0,
        "both secrets are resolved before any request"
    );
    let detail = body["detail"].as_str().unwrap();
    assert!(
        !detail.contains("absent-secret"),
        "no secret reference may leak: {detail}"
    );
    assert!(
        !detail.contains("oauth-secret"),
        "no secret material may leak: {detail}"
    );
}

#[tokio::test]
async fn tenants_do_not_share_a_cached_token() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/api")
            .header("authorization", "Bearer tok-shared");
        then.status(200).body("ok");
    });
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .body_includes("grant_type=client_credentials");
        then.status(200).body(token_body("tok-shared", 3600));
    });

    let harness = harness();
    let alias = "oauth-tenant.example.com";
    let auth = json!({
        "type": OAUTH_FORM,
        "config": {
            "token_endpoint": format!("http://127.0.0.1:{}/token", server.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "oauth-secret",
        },
    });
    // The same alias is registered once per tenant; each caller must mint its
    // own token because the cache key carries the tenant.
    for caller in [tenant(), common::parent()] {
        let created = json_body(
            post(
                harness.router(),
                "/oagw/v1/upstreams",
                json!({
                    "alias": alias,
                    "protocol": PROTOCOL,
                    "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                    "auth": auth,
                }),
                caller,
            )
            .await,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_owned();
        post(
            harness.router(),
            "/oagw/v1/routes",
            json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
            caller,
        )
        .await;
    }

    for caller in [tenant(), common::parent()] {
        let response = harness
            .send("GET", &format!("/oagw/v1/proxy/{alias}/api"), None, caller)
            .await;
        assert_eq!(response.status(), 200);
    }
    assert_eq!(token.calls(), 2, "the cache key is scoped per tenant");
    assert_eq!(target.calls(), 2);
}
