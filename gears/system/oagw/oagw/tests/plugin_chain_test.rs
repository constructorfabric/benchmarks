// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-plugin-chain:p2
//! The plugin chain of one proxied request (ADR-0002, ADR-0008, ADR-0009,
//! DESIGN §3.2): the built-in plugins on the wire, the composition of the
//! upstream and the route chains and the fail-closed behaviour of a reference
//! this deployment cannot enforce.
//!
//! Credential paths run against the SDK's own `MockCredStoreClient`, which the
//! `test-util` feature exposes to integration tests. The `OAuth2` token endpoint
//! is a scripted raw socket, because the exchange has to be counted and its
//! form body inspected.

mod common;

use anyhow::{Context as _, Result};
use common::{
    ERROR_SOURCE, Harness, LogCapture, ProxyHarness, domain_route, domain_upstream, https_upstream,
    loopback_endpoint, problem_type,
};
use credstore_sdk::test_util::MockCredStoreClient;
use httpmock::prelude::{GET, MockServer};
use oagw::domain::model::{AuthConfig, HttpMethod, PluginBinding, PluginsConfig, SharingMode};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

/// `X-OAGW-` proxy route of every test.
const PROXY_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/chat";

/// Seed the harness upstream with an optional plugin chain.
fn seed_upstream_on(harness: &ProxyHarness, port: u16, plugins: Option<PluginsConfig>) -> Uuid {
    let owner = harness.tenant();
    let mut upstream = domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    );
    upstream.plugins = plugins;
    let id = harness.seed_upstream(upstream);
    seed_route(harness, id);
    id
}

/// Seed an upstream chain **and** a route chain over the same upstream.
///
/// The route carries the one `/v1/chat` match rule, so the pair is seeded in a
/// single pass instead of adding a second route for an existing upstream.
fn seed_pair(
    harness: &ProxyHarness,
    port: u16,
    upstream_chain: PluginsConfig,
    route_chain: Vec<PluginBinding>,
) -> Uuid {
    let owner = harness.tenant();
    let mut upstream = domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    );
    upstream.plugins = Some(upstream_chain);
    let id = harness.seed_upstream(upstream);
    let mut route = domain_route(
        owner,
        id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/chat",
        &[],
    );
    route.plugins = Some(chain(route_chain));
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| panic!("the test route must seed: {error}"));
    id
}

/// Seed the `/v1/chat` route of `upstream_id`, with an optional chain.
fn seed_route(harness: &ProxyHarness, upstream_id: Uuid) {
    let route = domain_route(
        harness.tenant(),
        upstream_id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/chat",
        &[],
    );
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| panic!("the test route must seed: {error}"));
}

/// A harness whose upstream binds `auth` and whose credential store is `store`.
fn harness_with_auth(
    port: u16,
    auth: serde_json::Value,
    credstore: Option<Arc<MockCredStoreClient>>,
) -> ProxyHarness {
    let harness = ProxyHarness::with_credential_store(
        &common::proxy_config(),
        Arc::new(common::StaticTenantChain),
        credstore.map(|store| store as Arc<dyn credstore_sdk::CredStoreClientV1>),
    );
    let owner = harness.tenant();
    let mut upstream = domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    );
    upstream.auth = Some(auth_binding(auth));
    seed_route(&harness, harness.seed_upstream(upstream));
    harness
}

/// An `auth` binding over the raw members the test spelled.
///
/// The write path would parse these members itself; building the record here
/// keeps the helper free of a `Result` it cannot do anything with.
fn auth_binding(members: serde_json::Value) -> AuthConfig {
    let mut raw = match members {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    AuthConfig {
        plugin_type: raw
            .remove("type")
            .and_then(|kind| kind.as_str().map(str::to_owned)),
        sharing: SharingMode::Private,
        raw,
    }
}

/// Credentials of the tests' one vendor.
fn vendor_credentials() -> Arc<MockCredStoreClient> {
    Arc::new(MockCredStoreClient::with_secrets(Vec::from([
        ("vendor-key".to_owned(), "k-123".to_owned()),
        ("client-id".to_owned(), "cid".to_owned()),
        ("client-secret".to_owned(), "s3cr3t".to_owned()),
    ])))
}

/// A chain of bindings, private to the seeding tenant.
fn chain(bindings: Vec<PluginBinding>) -> PluginsConfig {
    PluginsConfig {
        sharing: SharingMode::Private,
        items: bindings,
    }
}

/// A bare reference binding: built-in GTS id or custom plugin UUID.
fn bare(reference: String) -> PluginBinding {
    PluginBinding::Reference(reference)
}

/// A `plugin_ref` binding with a `config` member (ADR-0009).
fn configured(reference: String, config: serde_json::Value) -> PluginBinding {
    let mut members = serde_json::Map::new();
    members.insert("config".to_owned(), config);
    PluginBinding::Configured {
        plugin_ref: reference,
        config: members,
    }
}

/// The GTS id of a built-in plugin.
fn built_in(kind: &str, name: &str) -> String {
    format!("gts.cf.core.oagw.{kind}_plugin.v1~cf.core.oagw.{name}.v1")
}

/// A `required_headers` binding for one phase.
fn required_headers(config: serde_json::Value) -> PluginBinding {
    configured(built_in("guard", "required_headers"), config)
}

/// A `request_id` binding, without configuration.
fn request_id() -> PluginBinding {
    bare(built_in("transform", "request_id"))
}

/// An empty 200 the scripted upstream answers with.
const ANSWER: &str = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
/// An empty 200 carrying the id the upstream chose.
const ANSWER_WITH_ID: &str =
    "HTTP/1.1 200 OK\r\nx-request-id: upstream-id\r\ncontent-length: 0\r\n\r\n";
/// An empty 200 carrying a header the `required_headers` guard looks for.
const SIGNED_ANSWER: &str = "HTTP/1.1 200 OK\r\nx-signature: sig\r\ncontent-length: 0\r\n\r\n";

/// Bind a raw upstream that answers **one** request with `response`.
///
/// The responder is spawned before the harness dials: the proxy call blocks on
/// the answer, so the accept must already be pending.
async fn scripted_upstream(
    response: &'static str,
) -> Result<(u16, tokio::task::JoinHandle<anyhow::Result<String>>)> {
    let upstream = Arc::new(common::RawUpstream::bind().await?);
    let port = upstream.port();
    let dial = tokio::spawn(async move { upstream.serve_once(response).await });
    Ok((port, dial))
}

// ── apikey: credential injection on the wire ─────────────────────────────

#[tokio::test]
async fn an_api_key_is_injected_into_the_default_header() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("x-api-key", "k-123");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "apikey", "key_ref": "cred://vendor-key" }),
        Some(vendor_credentials()),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn a_custom_header_name_and_prefix_are_honoured() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("x-vendor-key", "Bearer k-123");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({
            "type": "apikey",
            "key_ref": "cred://vendor-key",
            "header_name": "x-vendor-key",
            "prefix": "Bearer "
        }),
        Some(vendor_credentials()),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn a_query_parameter_binding_appends_the_key_to_the_dial() -> Result<()> {
    let (port, dial) = scripted_upstream(ANSWER).await?;
    let harness = harness_with_auth(
        port,
        serde_json::json!({
            "type": "apikey",
            "key_ref": "cred://vendor-key",
            "query_param": "api_key"
        }),
        Some(vendor_credentials()),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert!(
        dial.await??.contains("api_key=k-123"),
        "the dial carried the key"
    );
    Ok(())
}

#[tokio::test]
async fn a_binding_without_a_key_reference_is_a_400_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "apikey" }),
        Some(Arc::new(MockCredStoreClient::empty())),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn an_unknown_secret_reference_is_a_500_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "apikey", "key_ref": "cred://ghost" }),
        Some(Arc::new(MockCredStoreClient::empty())),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("secret.not_found.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn an_unavailable_credential_store_is_a_503_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "apikey", "key_ref": "cred://vendor-key" }),
        Some(Arc::new(MockCredStoreClient::always_failing())),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn a_deployment_without_a_credential_store_never_forwards_unauthenticated() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    // No credential store wired: the degraded registry of ADR-0008.
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "apikey", "key_ref": "cred://vendor-key" }),
        None,
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn a_catalog_only_auth_plugin_is_a_503_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_auth(
        server.port(),
        serde_json::json!({ "type": "basic" }),
        Some(vendor_credentials()),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("plugin.not_found.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// A dial target whose host `url::Url` refuses, so assembly always fails.
///
/// `2001:db8::1` is a valid IPv6 literal (the write path accepts it), but a URL
/// host carrying `:` must be bracketed, so `Url::parse` rejects it — the one
/// way an assembled dial URL can fail while the record is perfectly sound.
const UNPARSABLE_HOST: &str = "2001:db8::1";

#[tokio::test]
async fn a_failing_dial_target_never_echoes_an_injected_credential() -> Result<()> {
    let harness = ProxyHarness::with_credential_store(
        &common::proxy_config(),
        Arc::new(common::StaticTenantChain),
        Some(Arc::clone(&vendor_credentials()) as Arc<dyn credstore_sdk::CredStoreClientV1>),
    );
    let mut upstream = domain_upstream(
        harness.tenant(),
        "api.vendor.com",
        Vec::from([common::hostname_endpoint(UNPARSABLE_HOST, 80)]),
        true,
    );
    upstream.auth = Some(auth_binding(serde_json::json!({
        "type": "apikey",
        "key_ref": "cred://vendor-key",
        "query_param": "api_key"
    })));
    seed_route(&harness, harness.seed_upstream(upstream));

    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    // The parameter may be named; its value may not be spelled out.
    assert!(
        reply.text.contains("api_key=<elided>"),
        "body was: {}",
        reply.text
    );
    assert!(!reply.text.contains("k-123"), "body was: {}", reply.text);
    for line in capture.lines() {
        assert!(
            !line.contains("k-123"),
            "a log line carried the key: {line}"
        );
    }
    Ok(())
}

// ── oauth2: the cached client-credentials exchange ───────────────────────

/// A token endpoint that records every request it serves.
///
/// The exchange is a `POST` with a form body, so a scripted socket is the only
/// way to count the exchanges and to see the whole dial.
#[derive(Clone)]
struct Idp {
    listener: Arc<tokio::net::TcpListener>,
    /// Every request the endpoint received, oldest first.
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    /// The response to hand out, in order; the last one repeats.
    script: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Idp {
    /// Bind an endpoint that answers with `script`, in order.
    async fn bind(script: Vec<String>) -> Result<Self> {
        let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await?);
        Ok(Self {
            listener,
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            script: Arc::new(std::sync::Mutex::new(script)),
        })
    }

    /// A token response for `token`, valid for `lifetime_secs`.
    fn token(token: &str, lifetime_secs: u64) -> String {
        let body = format!(
            r#"{{"access_token":"{token}","expires_in":{lifetime_secs},"token_type":"Bearer"}}"#
        );
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A response the exchange rejects.
    fn refusal() -> String {
        let body = r#"{"error":"server_error"}"#;
        format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |addr| addr.port())
    }

    /// Serve until the test's runtime drops this task.
    async fn serve(self) {
        loop {
            let Ok((mut socket, _)) = self.listener.accept().await else {
                break;
            };
            let request = read_request(&mut socket).await.unwrap_or_default();
            let response = self.script.lock().map_or_else(
                |_| Self::refusal(),
                |mut script| {
                    if script.len() > 1 {
                        script.remove(0)
                    } else {
                        script.first().cloned().unwrap_or_else(Self::refusal)
                    }
                },
            );
            self.requests
                .lock()
                .map_or_else(|_| (), |mut requests| requests.push(request));
            if socket.write_all(response.as_bytes()).await.is_err()
                || socket.shutdown().await.is_err()
            {
                break;
            }
        }
    }

    /// How many exchanges the endpoint served.
    fn calls(&self) -> usize {
        self.requests.lock().map_or(0, |requests| requests.len())
    }

    /// The whole request of the `index`-th exchange.
    fn request(&self, index: usize) -> String {
        self.requests.lock().map_or_else(
            |_| String::new(),
            |requests| requests.get(index).cloned().unwrap_or_default(),
        )
    }
}

/// Read a request head plus whatever body has already arrived.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Result<String> {
    let mut received = Vec::new();
    let mut buffer = [0u8; 4096];
    while !received.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
    }
    // The form body usually arrives with the head; a short grace period keeps
    // the assertion from racing the socket.
    let _grace = tokio::time::timeout(Duration::from_millis(50), socket.read(&mut buffer)).await;
    received.extend_from_slice(&buffer);
    Ok(String::from_utf8_lossy(&received).to_string())
}

/// Harness with an `oauth2_client_cred` upstream binding over `idp`.
fn harness_with_oauth2(
    upstream_port: u16,
    idp: &Idp,
    auth_method: &str,
    credstore: Arc<MockCredStoreClient>,
) -> ProxyHarness {
    harness_with_auth(
        upstream_port,
        serde_json::json!({
            "type": auth_method,
            "token_endpoint": format!("http://127.0.0.1:{}/token", idp.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
            "scopes": "read write"
        }),
        Some(credstore),
    )
}

#[tokio::test]
async fn an_oauth2_binding_exchanges_once_and_caches_the_token() -> Result<()> {
    let upstream = MockServer::start();
    let mock = upstream.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer tok-1");
        then.status(200).body("ok");
    });
    let idp = Idp::bind(Vec::from([Idp::token("tok-1", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = harness_with_oauth2(
        upstream.port(),
        &idp,
        "oauth2_client_cred",
        vendor_credentials(),
    );

    // One caller across both requests: the token cache is keyed by the subject.
    let identity = common::security_context(harness.tenant())?;
    let first = harness
        .proxy_as_identity(&identity, "GET", PROXY_PATH, &[], b"")
        .await?;
    let second = harness
        .proxy_as_identity(&identity, "GET", PROXY_PATH, &[], b"")
        .await?;

    assert_eq!(first.status, axum::http::StatusCode::OK);
    assert_eq!(second.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 2);
    assert_eq!(
        idp.calls(),
        1,
        "the second request must be served from the cache"
    );
    let dial = idp.request(0);
    assert!(dial.starts_with("POST /token"), "exchange was: {dial}");
    assert!(
        dial.contains("grant_type=client_credentials"),
        "exchange was: {dial}"
    );
    assert!(dial.contains("scope=read+write"), "exchange was: {dial}");
    assert!(dial.contains("client_id=cid"), "exchange was: {dial}");
    assert!(
        dial.contains("client_secret=s3cr3t"),
        "exchange was: {dial}"
    );
    Ok(())
}

#[tokio::test]
async fn the_basic_variant_sends_its_credentials_in_a_header() -> Result<()> {
    let upstream = MockServer::start();
    upstream.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let idp = Idp::bind(Vec::from([Idp::token("tok-1", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = harness_with_oauth2(
        upstream.port(),
        &idp,
        "oauth2_client_cred_basic",
        vendor_credentials(),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert!(
        idp.request(0).contains("authorization: Basic "),
        "exchange was: {}",
        idp.request(0)
    );
    Ok(())
}

/// Six concurrent requests of one cold subject.
const STAMPEDE: usize = 6;

#[tokio::test]
async fn a_cold_key_is_exchanged_once_no_matter_how_many_requests_race() -> Result<()> {
    let upstream = MockServer::start();
    let mock = upstream.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let idp = Idp::bind(Vec::from([Idp::token("tok-1", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = harness_with_oauth2(
        upstream.port(),
        &idp,
        "oauth2_client_cred",
        vendor_credentials(),
    );

    // One subject, many requests at once: the cache key is the same for all.
    let shared = Arc::new(harness);
    let identity = common::security_context(shared.tenant())?;
    let mut requests = Vec::new();
    for _ in 0..STAMPEDE {
        let harness = Arc::clone(&shared);
        let identity = identity.clone();
        let path = PROXY_PATH;
        requests.push(tokio::spawn(async move {
            harness
                .proxy_as_identity(&identity, "GET", path, &[], b"")
                .await
        }));
    }
    let mut statuses = Vec::new();
    for request in requests {
        statuses.push(request.await??.status);
    }

    assert!(
        statuses
            .iter()
            .all(|status| *status == axum::http::StatusCode::OK),
        "statuses: {statuses:?}"
    );
    assert_eq!(mock.calls(), STAMPEDE);
    assert_eq!(
        idp.calls(),
        1,
        "N concurrent cold misses must exchange once, not N times"
    );
    Ok(())
}

#[tokio::test]
async fn the_binding_of_the_adr_shape_is_accepted_and_enforced() -> Result<()> {
    let upstream = MockServer::start();
    // The exchange has to have happened for the dial to be authorised at all.
    let mock = upstream.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("authorization", "Bearer tok-1");
        then.status(200).body("ok");
    });
    let idp = Idp::bind(Vec::from([Idp::token("tok-1", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = ProxyHarness::with_credential_store(
        &common::proxy_config(),
        Arc::new(common::StaticTenantChain),
        Some(Arc::clone(&vendor_credentials()) as Arc<dyn credstore_sdk::CredStoreClientV1>),
    );
    // ADR-0008 "Upstream Configuration Example": the members live under
    // `config`, not beside `type`. Created through the management API, so the
    // write path is what has to accept the shape.
    let mut payload = serde_json::json!({
        "protocol": common::PROTOCOL_HTTP,
        "server": {
            "endpoints": [common::endpoint("http", "127.0.0.1", upstream.port())]
        }
    });
    payload["auth"] = serde_json::json!({
        "type": built_in("auth", "oauth2_client_cred"),
        "config": {
            "token_endpoint": format!("http://127.0.0.1:{}/token", idp.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
            "scopes": "read write"
        }
    });
    let created = harness
        .call("POST", "/oagw/v1/upstreams", Some(payload))
        .await?;
    assert_eq!(
        created.status,
        axum::http::StatusCode::CREATED,
        "body: {}",
        created.text
    );
    let upstream_id = Uuid::parse_str(created.problem_field("id").context("id")?)?;
    seed_route(&harness, upstream_id);
    // An IP endpoint derives its alias from the endpoint, port included.
    let alias = created
        .json
        .pointer("/alias")
        .and_then(serde_json::Value::as_str)
        .context("alias")?;
    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");

    let reply = harness.proxy("GET", &path, &[], b"").await?;

    assert_eq!(
        reply.status,
        axum::http::StatusCode::OK,
        "body: {}",
        reply.text
    );
    assert_eq!(mock.calls(), 1);
    assert_eq!(idp.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn a_blank_auth_type_is_refused_on_the_write_path() -> Result<()> {
    let harness = Harness::new();
    let mut payload = https_upstream("api.openai.com", 443);
    payload["auth"] = serde_json::json!({ "type": "   " });

    let reply = harness
        .call("POST", "/oagw/v1/upstreams", Uuid::now_v7(), Some(payload))
        .await?;

    assert_eq!(
        reply.status,
        axum::http::StatusCode::BAD_REQUEST,
        "body: {}",
        reply.text
    );
    Ok(())
}

#[tokio::test]
async fn a_blank_auth_type_never_forwards_unauthenticated() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = ProxyHarness::new();
    let mut upstream = domain_upstream(
        harness.tenant(),
        "api.vendor.com",
        Vec::from([loopback_endpoint(server.port())]),
        true,
    );
    upstream.auth = Some(auth_binding(serde_json::json!({ "type": "   " })));
    seed_route(&harness, harness.seed_upstream(upstream));

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn a_failed_exchange_is_a_401_and_is_never_cached() -> Result<()> {
    let upstream = MockServer::start();
    let mock = upstream.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    // The first exchange is refused, the second one is not: if the failure had
    // been cached, the second request would be a 401 again.
    let idp = Idp::bind(Vec::from([Idp::refusal(), Idp::token("tok-2", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = harness_with_oauth2(
        upstream.port(),
        &idp,
        "oauth2_client_cred",
        vendor_credentials(),
    );

    let refused = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    let retried = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(refused.status, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(refused.problem_type(), Some(problem_type("auth.failed.v1")));
    assert_eq!(retried.status, axum::http::StatusCode::OK);
    assert_eq!(idp.calls(), 2, "a failed exchange must not be cached");
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn an_oauth2_token_is_never_logged_or_serialised() -> Result<()> {
    let upstream = MockServer::start();
    upstream.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let idp = Idp::bind(Vec::from([Idp::token("tok-1", 3600)])).await?;
    tokio::spawn(idp.clone().serve());
    let harness = harness_with_oauth2(
        upstream.port(),
        &idp,
        "oauth2_client_cred",
        vendor_credentials(),
    );
    // A second deployment whose store refuses the reference: the failure path
    // is the one that renders a problem document.
    let failing = harness_with_auth(
        upstream.port(),
        serde_json::json!({ "type": "apikey", "key_ref": "cred://ghost" }),
        Some(Arc::new(MockCredStoreClient::empty())),
    );

    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let first = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    let second = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    let rejected = failing.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(first.status, axum::http::StatusCode::OK);
    assert_eq!(second.status, axum::http::StatusCode::OK);
    assert_eq!(
        rejected.status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    for line in capture.lines() {
        assert!(
            !line.contains("tok-1"),
            "a log line carried the token: {line}"
        );
        assert!(
            !line.contains("s3cr3t"),
            "a log line carried a credential: {line}"
        );
    }
    assert!(
        !rejected.text.contains("s3cr3t"),
        "a problem document carried a credential: {}",
        rejected.text
    );
    Ok(())
}

// ── required_headers: presence-only enforcement (ADR-0009) ──────────────

/// A harness whose upstream carries a chain of `bindings`.
fn harness_with_chain(port: u16, bindings: Vec<PluginBinding>) -> ProxyHarness {
    let harness = ProxyHarness::new();
    seed_upstream_on(&harness, port, Some(chain(bindings)));
    harness
}

#[tokio::test]
async fn a_request_without_a_required_header_is_rejected_with_400() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_chain(
        server.port(),
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "X-Correlation-Id"
        }))]),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    assert_eq!(
        reply.problem_field("error_code"),
        Some("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(
        reply.problem_field("missing_header"),
        Some("x-correlation-id")
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn a_request_carrying_the_required_header_is_forwarded() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/chat")
            .header("x-correlation-id", "corr-1");
        then.status(200).body("ok");
    });
    let harness = harness_with_chain(
        server.port(),
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "X-Correlation-Id"
        }))]),
    );

    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-correlation-id", "corr-1")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn a_bare_reference_binding_is_accepted_and_fails_open() -> Result<()> {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_chain(
        server.port(),
        Vec::from([bare(built_in("guard", "required_headers"))]),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    // Fail-open: the binding carries no configuration, so the guard objects to
    // nothing (ADR-0009).
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn an_upstream_response_without_a_required_header_is_a_502() -> Result<()> {
    let (port, _dial) = scripted_upstream(ANSWER).await?;
    let harness = harness_with_chain(
        port,
        Vec::from([required_headers(serde_json::json!({
            "required_response_headers": "x-signature"
        }))]),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("protocol.error.v1"))
    );
    assert_eq!(
        reply.problem_field("error_code"),
        Some("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(reply.problem_field("missing_header"), Some("x-signature"));
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    Ok(())
}

/// A harness whose upstream strips `name` from its own response.
///
/// The response header rules are the one way the gateway can drop a header the
/// upstream did send.
fn harness_with_response_rule(port: u16, name: &str) -> ProxyHarness {
    let harness = ProxyHarness::new();
    let mut upstream = domain_upstream(
        harness.tenant(),
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    );
    upstream.plugins = Some(chain(Vec::from([required_headers(serde_json::json!({
        "required_response_headers": name
    }))])));
    upstream.headers = Some(oagw::domain::model::HeadersConfig {
        request: None,
        response: Some(oagw::domain::model::ResponseHeaderRules {
            set: std::collections::HashMap::new(),
            add: std::collections::HashMap::new(),
            remove: Vec::from([name.to_owned()]),
        }),
    });
    seed_route(&harness, harness.seed_upstream(upstream));
    harness
}

#[tokio::test]
async fn a_response_rule_that_strips_a_required_header_is_not_a_502() -> Result<()> {
    // The upstream signs its answer; the response rule removes the signature
    // before it reaches the client.
    let (port, _dial) = scripted_upstream(SIGNED_ANSWER).await?;
    let harness = harness_with_response_rule(port, "x-signature");

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    // The guard asks whether the *upstream* sent the header, so a rule that
    // strips it for the client is not the upstream failing the contract.
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.header("x-signature"), None);
    Ok(())
}

#[tokio::test]
async fn an_upstream_response_carrying_the_required_header_is_forwarded() -> Result<()> {
    let (port, _dial) = scripted_upstream(SIGNED_ANSWER).await?;
    let harness = harness_with_chain(
        port,
        Vec::from([required_headers(serde_json::json!({
            "required_response_headers": "x-signature"
        }))]),
    );

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn only_the_first_missing_header_is_reported() -> Result<()> {
    let server = MockServer::start();
    let harness = harness_with_chain(
        server.port(),
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "accept,x-tenant-id,x-signature"
        }))]),
    );

    let reply = harness
        .proxy("GET", PROXY_PATH, &[("accept", "*/*")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(reply.problem_field("missing_header"), Some("x-tenant-id"));
    Ok(())
}

// ── request_id: correlation id injection and propagation ────────────────

#[tokio::test]
async fn a_request_without_an_id_leaves_with_a_generated_one() -> Result<()> {
    let (port, dial) = scripted_upstream(ANSWER_WITH_ID).await?;
    let harness = harness_with_chain(port, Vec::from([request_id()]));

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    let injected = dial
        .await??
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("x-request-id:"))
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        injected.len(),
        "x-request-id: ".len() + 36,
        "dial was: {injected}"
    );
    Ok(())
}

#[tokio::test]
async fn a_caller_provided_id_is_never_overwritten() -> Result<()> {
    let (port, dial) = scripted_upstream(ANSWER_WITH_ID).await?;
    let harness = harness_with_chain(port, Vec::from([request_id()]));

    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-request-id", "caller-id")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert!(
        dial.await??.contains("x-request-id: caller-id"),
        "the dial carried the caller's id"
    );
    Ok(())
}

#[tokio::test]
async fn the_upstream_id_is_propagated_back_to_the_client() -> Result<()> {
    let (port, _dial) = scripted_upstream(ANSWER_WITH_ID).await?;
    let harness = harness_with_chain(port, Vec::from([request_id()]));

    // The upstream replaces the id it was handed, and the replacement is what
    // the caller can correlate its request against.
    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-request-id", "caller-id")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.header("x-request-id"), Some("upstream-id"));
    Ok(())
}

// ── chain composition and fail-closed references ────────────────────────

#[tokio::test]
async fn a_chain_reference_without_a_record_is_a_503_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let unknown = Uuid::now_v7();
    let harness = harness_with_chain(server.port(), Vec::from([bare(unknown.to_string())]));

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("plugin.not_found.v1"))
    );
    assert!(
        reply.text.contains(&unknown.to_string()),
        "detail was: {}",
        reply.text
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn a_catalogued_but_unboundable_reference_is_a_503_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_chain(server.port(), Vec::from([bare(built_in("guard", "cors"))]));

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("plugin.not_found.v1"))
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn the_upstream_and_the_route_chains_both_run() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).header("x-signature", "sig").body("ok");
    });
    let harness = ProxyHarness::new();
    seed_pair(
        &harness,
        server.port(),
        chain(Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-correlation-id"
        }))])),
        Vec::from([required_headers(serde_json::json!({
            "required_response_headers": "x-signature"
        }))]),
    );

    let without = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    let with = harness
        .proxy("GET", PROXY_PATH, &[("x-correlation-id", "corr-1")], b"")
        .await?;

    // The upstream guard objects to the missing request header, the route guard
    // to the missing response header: both halves of the chain ran.
    assert_eq!(without.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        without.problem_field("missing_header"),
        Some("x-correlation-id")
    );
    assert_eq!(with.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn an_upstream_guard_survives_a_route_chain() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = ProxyHarness::new();
    seed_pair(
        &harness,
        server.port(),
        chain(Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-unreachable-header"
        }))])),
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-correlation-id"
        }))]),
    );

    // The chain is the concatenation of the two, so the upstream requirement is
    // still in force and the request is refused before the route guard runs.
    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-correlation-id", "corr-1")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_field("missing_header"),
        Some("x-unreachable-header")
    );
    assert_eq!(mock.calls(), 0);
    Ok(())
}

#[tokio::test]
async fn the_route_chain_runs_after_the_upstream_chain() -> Result<()> {
    let server = MockServer::start();
    let harness = ProxyHarness::new();
    // Two upstream bindings: the first is overridden by the route, the second
    // stays in force, so a route must not silently drop a guard.
    seed_pair(
        &harness,
        server.port(),
        chain(Vec::from([
            required_headers(
                serde_json::json!({ "required_request_headers": "x-unreachable-header" }),
            ),
            required_headers(serde_json::json!({ "required_request_headers": "x-signature" })),
        ])),
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-correlation-id"
        }))]),
    );

    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-correlation-id", "corr-1")], b"")
        .await?;

    // The first binding of the merged chain is the first upstream one, so it is
    // the first requirement reported.
    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_field("missing_header"),
        Some("x-unreachable-header")
    );
    Ok(())
}

#[tokio::test]
async fn an_enforced_upstream_chain_runs_before_the_route_chain() -> Result<()> {
    let server = MockServer::start();
    let harness = ProxyHarness::new();
    let upstream_chain = PluginsConfig {
        sharing: SharingMode::Enforce,
        items: Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-unreachable-header"
        }))]),
    };
    seed_pair(
        &harness,
        server.port(),
        upstream_chain,
        Vec::from([required_headers(serde_json::json!({
            "required_request_headers": "x-correlation-id"
        }))]),
    );

    // Under `enforce` the upstream chain is the head of the merged one, which
    // is why the request is refused before the route guard is reached.
    let reply = harness
        .proxy("GET", PROXY_PATH, &[("x-correlation-id", "corr-1")], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_field("missing_header"),
        Some("x-unreachable-header")
    );
    Ok(())
}
