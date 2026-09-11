//! Integration tests of credential isolation across every surface
//! (`cpt-cf-oagw-dod-plugin-system-credential-isolation`).
//!
//! The guarantee under test is the *surface* one: no log record, no rendered
//! error body and no management API response carries resolved secret material,
//! and only the `cred://` reference crosses any of them. The in-memory
//! lifetime of the two residual-plaintext surfaces ADR 0008 records as
//! exceptions — the injected `Authorization`/`x-api-key` header string and the
//! transient token inside the IdP fetch — is deliberately not asserted here.
//!
//! The material is planted in a `credstore` test double the request-time
//! resolution of entry 2.6 resolves, and every resolution is counted, so an
//! assertion that the material never surfaced is never vacuous: the test that
//! drives an error through the proxy first proves the credential *was*
//! resolved.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-credential-isolation:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use credstore_sdk::test_util::MockCredStoreClient;
use credstore_sdk::{CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef};
use oagw::test_support::{
    permissive_surface, permissive_surface_with_credstore, route_for, seed_route, seed_upstream,
    upstream_at,
};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const SECRET: &str = "sk-live-9c41-partner-openai-resolved-material";
const REFERENCE: &str = "cred://partner-openai-key";
const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// The `credstore` double the tests seed: the reference resolves to `SECRET`,
/// and every resolution is counted so a test can prove the material was
/// resolved in the very request whose surfaces it inspects.
struct CountingCredStore {
    inner: MockCredStoreClient,
    resolutions: AtomicUsize,
}

impl CountingCredStore {
    fn seeded() -> Arc<Self> {
        Arc::new(Self {
            inner: MockCredStoreClient::with_secrets(vec![(REFERENCE.to_owned(), SECRET.to_owned())]),
            resolutions: AtomicUsize::new(0),
        })
    }

    fn resolutions(&self) -> usize {
        self.resolutions.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CredStoreClientV1 for CountingCredStore {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        CredStoreClientV1::get(&self.inner, ctx, key).await
    }
}

/// An upstream record carrying the `apikey` auth plugin bound to `REFERENCE`.
fn record_with_auth(tenant: Uuid, host: &str, port: u16) -> oagw::domain::dto::Upstream {
    let mut record = upstream_at(tenant, "api.vendor.com", oagw::EndpointScheme::Http, host, port);
    record.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some(APIKEY.to_owned()),
        config: Some(json!({ "api_key_ref": REFERENCE })),
        ..oagw::AuthConfig::default()
    });
    record
}

fn upstream_body(port: u16) -> Value {
    json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": "api.vendor.com", "port": port } ] },
        "auth": { "type": APIKEY, "config": { "api_key_ref": REFERENCE } }
    })
}

/// A `tracing` subscriber that captures every event field it is handed.
///
/// The crate's log surface is the `tracing` macro set, so the assertion is
/// made against the records the runtime actually emits rather than against the
/// source text.
#[derive(Default)]
struct LogCapture {
    lines: std::sync::Mutex<Vec<String>>,
}

impl LogCapture {
    fn rendered(&self) -> Vec<String> {
        self.lines.lock().expect("log lines").clone()
    }
}

struct CaptureSubscriber {
    capture: Arc<LogCapture>,
}

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let _ = std::fmt::Write::write_fmt(self.0, format_args!(" {}={}", field.name(), value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = std::fmt::Write::write_fmt(self.0, format_args!(" {}={:?}", field.name(), value));
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::Id {
        tracing::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::new();
        values.record(&mut FieldWriter(&mut line));
        self.capture.lines.lock().expect("log lines").push(line);
    }

    fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("[{}]", event.metadata().level());
        event.record(&mut FieldWriter(&mut line));
        self.capture.lines.lock().expect("log lines").push(line);
    }

    fn enter(&self, _span: &tracing::Id) {}

    fn exit(&self, _span: &tracing::Id) {}

    fn clone_span(&self, id: &tracing::Id) -> tracing::Id {
        id.clone()
    }

    fn try_close(&self, _id: tracing::Id) -> bool {
        true
    }

}

/// Install the process-wide log capture. `tracing` caches a callsite's
/// interest against the subscriber that first registers it, so every test in
/// this file installs the same subscriber before it emits anything.
fn install_capture() -> Arc<LogCapture> {
    static CAPTURE: std::sync::OnceLock<Arc<LogCapture>> = std::sync::OnceLock::new();
    let capture = CAPTURE.get_or_init(|| {
        let capture = Arc::new(LogCapture::default());
        let _ = tracing::subscriber::set_global_default(CaptureSubscriber {
            capture: Arc::clone(&capture),
        });
        capture
    });
    Arc::clone(capture)
}

/// The management API carries only the reference, never the resolved value.
#[tokio::test]
async fn the_management_api_returns_only_the_reference() {
    install_capture();
    let credstore = CountingCredStore::seeded();
    let surface = permissive_surface_with_credstore(None, Arc::clone(&credstore) as _).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();

    // The write path accepts the reference and returns the record it stored.
    let (status, bytes) = surface
        .create(tenant, principal, upstream_body(8443))
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let created: Value = serde_json::from_slice(&bytes).expect("record");
    let upstream_id: Uuid = serde_json::from_value(created["id"].clone()).expect("identifier");
    assert!(
        !bytes_to_string(&bytes).contains(SECRET),
        "the create response never carries the resolved value: {bytes:?}"
    );
    assert!(
        bytes_to_string(&bytes).contains(REFERENCE),
        "the reference itself is the only credential surface: {bytes:?}"
    );

    // A custom plugin whose registered source names nothing secret, so the
    // plugin surfaces carry the registered content and nothing resolved.
    let plugin = json!({
        "name": "partner-guard",
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~",
        "source_code": "pub fn guard(request: &Request) -> Decision { Decision::Allow }"
    });
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(oagw::test_support::security_context(tenant, principal)),
            Some(plugin),
        )
        .await;
    assert_eq!(status, 201, "{bytes:?}");
    let plugin_id: Uuid = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("identifier")
        .parse()
        .expect("uuid");

    // Every read surface of every record that exists: the upstream, its list,
    // the plugin, its source, and the routes beneath it.
    let mut bodies: Vec<String> = Vec::new();
    for (method, path) in [
        (http::Method::GET, format!("/oagw/v1/upstreams/{upstream_id}")),
        (http::Method::GET, "/oagw/v1/upstreams".to_owned()),
        (http::Method::GET, "/oagw/v1/routes".to_owned()),
        (http::Method::GET, "/oagw/v1/plugins".to_owned()),
        (http::Method::GET, format!("/oagw/v1/plugins/{plugin_id}")),
        (http::Method::GET, format!("/oagw/v1/plugins/{plugin_id}/source")),
    ] {
        let (status, bytes) = surface
            .send(method.clone(), &path, Some(oagw::test_support::security_context(tenant, principal)), None)
            .await;
        assert_eq!(status, 200, "{method} {path}: {bytes:?}");
        bodies.push(bytes_to_string(&bytes));
    }
    for body in &bodies {
        assert!(!body.contains(SECRET), "a management response carried resolved material: {body}");
        assert!(!body.contains("sk-live-"), "{body}");
    }
    // Nothing was resolved: the management path never calls `cred_store`.
    assert_eq!(credstore.resolutions(), 0, "the management path resolves no credential");
}

fn bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

/// A rendered error body carries no resolved material, although the credential
/// was resolved during the very request that failed.
#[tokio::test]
async fn a_rendered_error_body_carries_no_resolved_material() {
    install_capture();
    let credstore = CountingCredStore::seeded();
    let surface = permissive_surface_with_credstore(
        Some(json!({ "allow_http_upstream": true, "proxy_timeout_secs": 5 })),
        Arc::clone(&credstore) as _,
    )
    .await;
    // A port the proxy cannot connect to, so the failure happens *after* the
    // auth phase resolved the credential.
    let closed = bind_then_drop();
    let tenant = Uuid::new_v4();
    let upstream_id = seed_upstream(&surface, record_with_auth(tenant, &closed.0, closed.1));
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[oagw::HttpMethod::Get]));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert!(
        exchange.status.is_server_error() || exchange.status.is_client_error(),
        "{:?}",
        exchange.text()
    );
    let rendered = exchange.text();
    assert!(
        !rendered.contains(SECRET),
        "the rendered error body carried resolved material: {rendered}"
    );
    for (name, value) in &exchange.headers {
        assert!(!value.contains(SECRET), "{name}: {value}");
    }
    assert!(
        credstore.resolutions() >= 1,
        "the test is only meaningful when the credential was resolved"
    );
}

/// A port that accepted a connection once and now refuses them.
fn bind_then_drop() -> (String, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the listener binds");
    let address = listener.local_addr().expect("the address");
    drop(listener);
    ("127.0.0.1".to_owned(), address.port())
}

/// No log record the runtime emits carries resolved material.
#[tokio::test]
async fn no_log_record_carries_resolved_material() {
    let capture = install_capture();
    let credstore = CountingCredStore::seeded();
    let surface = permissive_surface_with_credstore(
        Some(json!({ "allow_http_upstream": true, "proxy_timeout_secs": 5 })),
        Arc::clone(&credstore) as _,
    )
    .await;
    // The endpoint pool points at a port that now refuses connections, so the
    // proxied request resolves the credential and then fails.
    let closed = bind_then_drop();
    let tenant = Uuid::new_v4();
    let upstream_id = seed_upstream(&surface, record_with_auth(tenant, &closed.0, closed.1));
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[oagw::HttpMethod::Get]));

    // A management write emits its audit record, and the proxied request
    // resolves the credential and then fails on the closed upstream.
    let principal = Uuid::new_v4();
    let _ = surface.create(tenant, principal, upstream_body(8443)).await;
    let _ = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;

    let rendered = capture.rendered();
    assert!(
        !rendered.is_empty(),
        "the capture is only meaningful over the records the runtime emits"
    );
    assert!(
        !rendered.iter().any(|record| record.contains(SECRET)),
        "a log record carried resolved material: {rendered:?}"
    );
    assert!(credstore.resolutions() >= 1, "the credential was resolved during the request");
}

/// A reference the store does not resolve is the `500`
/// `secret.not_found.v1` gateway outcome, never a `401`, and it is stamped
/// `X-OAGW-Error-Source: gateway` because the credential store is the gear's
/// dependency (`cpt-cf-oagw-dod-request-proxy-plugin-chain-points`).
#[tokio::test]
async fn an_unresolvable_reference_is_the_gateway_sourced_secret_outcome() {
    install_capture();
    // The default double resolves nothing, so every reference is unresolvable.
    let surface = permissive_surface(Some(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5
    })))
    .await;
    let tenant = Uuid::new_v4();
    let upstream_id = seed_upstream(&surface, record_with_auth(tenant, "127.0.0.1", 1));
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[oagw::HttpMethod::Get]));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::INTERNAL_SERVER_ERROR, "{:?}", exchange.text());
    assert!(
        exchange.text().contains("secret.not_found"),
        "the failure names the secret outcome: {:?}",
        exchange.text()
    );
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
}
