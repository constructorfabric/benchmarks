#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the observability surface of entry 2.7.
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests boot the mounted router
//! the way `tests/proxy_engine.rs` does and drive a proxied request through it,
//! then read what the gear emitted: the audit line the writer drained and the
//! series the metric registry recorded for the same closed request. The
//! assertions cover the fourteen-key line contract, the failure line, the
//! redaction of the line, the request families and the in-flight and routing
//! series of `cpt-cf-oagw-dod-observability-test-coverage`.

// @cpt-begin:cpt-cf-oagw-dod-observability-test-coverage:p1:inst-full
// This feature is covered by in-crate Rust tests only: the unit tests in the
// `#[cfg(test)]` modules of `src/infra/obs/` assert the fourteen keys in order
// and `error_type` `null` on a success, the GTS type identifier and the added
// `error_message` on a failure, the absence of any body, query string, header
// value or secret from an emitted line, the level vocabulary of DESIGN §4.3,
// the recorded name, kind and label keys of every family, the twelve histogram
// buckets and the cardinality rules; the integration tests here drive a
// proxied request through the mounted router and read what the gear emitted.
// @cpt-end:cpt-cf-oagw-dod-observability-test-coverage:p1:inst-full

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{Router, body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::routes::{MOUNT_ROOT, register_routes_with_config, register_routes_with_plugins};
use oagw::config::OagwConfig;
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::{FlatHierarchy, StaticHierarchy, TenantHierarchy};
use oagw::infra::obs::{
    AUDIT_EVENT, AUDIT_KEYS, KEY_ERROR_MESSAGE, Observability, RequestObservation,
    SUCCESS_SAMPLE_RATE,
};
use oagw::infra::obs::metrics::{
    BREAKER_TRANSITIONS, DURATION_BUCKETS, ERRORS_TOTAL, LABEL_ENDPOINT_HOST, LABEL_ERROR_TYPE,
    LABEL_FROM_STATE, LABEL_HOST, LABEL_METHOD, LABEL_OTHER, LABEL_PATH, LABEL_PHASE, LABEL_ROUTE,
    LABEL_SELECTION_METHOD, LABEL_STATUS, LABEL_TO_STATE, LABEL_UPSTREAM_ID,
    PHASE_GATEWAY_ADDED, PHASE_UPSTREAM, RATE_LIMIT_EXCEEDED, RATE_LIMIT_USAGE_RATIO,
    REQUEST_DURATION, REQUESTS_TOTAL, ROUTING_ENDPOINT_SELECTED, STATE_CLOSED, STATE_HALF_OPEN,
    STATE_OPEN,
};
use oagw::infra::obs::{MetricRegistry, RuntimeState};
use oagw::infra::proxy::breaker::FAILURE_THRESHOLD;
use oagw::infra::proxy::context::RequestContext;
use oagw::infra::proxy::{BreakerState, CallOutcome, CircuitBreaker, Transition};
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000050");
const ANCESTOR: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000002");

/// The alias the loopback stub upstream is stored under.
const ALIAS: &str = "stub.internal";

/// The path the test routes match.
const ROUTE_PATH: &str = "/v1";

/// Every scope a management caller may need.
const ALL: &[&str] = &["*"];

/// The upper bound the line waits are given: a bound long enough for the drain
/// task of a loaded test host, short enough that a broken emission fails fast.
const EMISSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Host OpenAPI registry double that records nothing.
#[derive(Default)]
struct NoopRegistry;

impl toolkit::api::OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mounted router over a fresh store and the given configuration.
fn mounted_with(config: &OagwConfig) -> Router {
    mounted_over(config, Arc::new(FlatHierarchy))
}

/// The mounted router over a fresh store, a configuration and a hierarchy.
fn mounted_over(config: &OagwConfig, hierarchy: Arc<dyn TenantHierarchy>) -> Router {
    register_routes_with_config(
        Router::new(),
        &NoopRegistry,
        Arc::new(OagwStore::new()),
        hierarchy,
        config,
    )
    .expect("the test configuration builds the proxy client")
}

/// The mounted router with the plugin chains the running gear builds.
///
/// The rate-limit stage is a chain stage, so the families that record its
/// decision are only reachable through the mount that carries the executor.
fn mounted_with_chains(config: &OagwConfig) -> Router {
    let chains: Arc<dyn oagw::infra::proxy::hooks::PluginChains> = Arc::new(
        oagw::infra::proxy::ChainExecutor::new(
            config,
            oagw::infra::proxy::credentials::CredentialSource::unresolved(),
        )
        .expect("the chain builds"),
    );
    register_routes_with_plugins(
        Router::new(),
        &NoopRegistry,
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
        config,
        Some(chains),
    )
    .expect("the router mounts")
}

/// The plaintext-opting configuration the stub upstream needs.
fn proxy_config(timeout: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: timeout,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid, scopes: &[&str]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(scopes.iter().map(|scope| (*scope).to_owned()).collect())
        .build()
        .expect("context builds")
}

/// Send a proxy request with the given headers and body.
async fn call(
    router: Router,
    method: &str,
    uri: &str,
    caller: Option<SecurityContext>,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(caller) = caller {
        builder = builder.extension(caller);
    }
    router
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_owned()))
                .expect("request builds"),
        )
        .await
        .expect("request serves")
}

/// Send an authenticated proxy request without extra headers.
async fn proxy(router: Router, method: &str, uri: &str) -> axum::response::Response {
    call(router, method, uri, Some(context(TENANT, ALL)), &[], None).await
}

/// The whole body of a response as a string.
async fn text(response: axum::response::Response) -> String {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

/// The whole body of a response as a JSON document.
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_str(&text(response).await).expect("body is a JSON document")
}

/// The body of a plaintext stub upstream stored as `alias`.
fn stub_upstream(alias: Option<&str>, port: u16) -> Value {
    // `passthrough: all` is declared, because the schema default of
    // `headers.request.passthrough` is `none`: a gateway that declares no
    // header disposition forwards no client header at all.
    let mut body = json!({
        "server": {
            "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ]
        },
        "protocol": PROTOCOL_HTTP,
        "headers": { "request": { "passthrough": "all" } }
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

/// Create an upstream through the management API.
async fn create_upstream(router: &Router, tenant: Uuid, body: Value) -> Value {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(tenant, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}

/// Create a route through the management API.
async fn create_route(router: &Router, tenant: Uuid, body: Value) {
    let response = call(
        router.clone(),
        "POST",
        &format!("{MOUNT_ROOT}/routes"),
        Some(context(tenant, ALL)),
        &[("content-type", "application/json")],
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// A route body matching every forwarded method on `path`.
fn route_body(upstream: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream,
        "match": { "http": {
            "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
            "path": path
        } }
    })
}

/// The identifier of the one upstream the calling tenant holds.
async fn only_upstream_id(router: &Router) -> Uuid {
    let response = call(
        router.clone(),
        "GET",
        &format!("{MOUNT_ROOT}/upstreams"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    let page = json_body(response).await;
    let record = page.as_array().expect("page").first().expect("record");
    Uuid::parse_str(record["id"].as_str().expect("id")).expect("uuid")
}

/// The label values of one series, as the read API takes them.
fn labels(values: &[(String, String)]) -> Vec<(&'static str, &str)> {
    values
        .iter()
        .map(|(key, value)| {
            let key: &'static str = Box::leak(key.clone().into_boxed_str());
            (key, value.as_str())
        })
        .collect()
}

/// The alias of the test's own stub upstream.
///
/// The observation layer is one instance per process, so the series the tests
/// read are shared by the whole test binary: every test stores its upstream
/// under its own alias, so the label sets it asserts on are its own.
fn alias_for(tag: &str) -> String {
    format!("{tag}.internal")
}

/// Seed the loopback stub under `alias` with its route, and return the proxy
/// path the route serves.
async fn stubbed(router: &Router, server: &MockServer, alias: &str) -> String {
    let stored = create_upstream(router, TENANT, stub_upstream(Some(alias), server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(router, TENANT, route_body(upstream, ROUTE_PATH)).await;
    format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}")
}

/// A `200` stub upstream stored under the given path.
async fn stub_ok<'a>(server: &'a MockServer, path: &'a str, body: &'a str) -> httpmock::Mock<'a> {
    server
        .mock_async(|when, then| {
            when.method("GET").path(path);
            then.status(200)
                .header("x-upstream", "stub")
                .header("content-type", "application/json")
                .body(body);
        })
        .await
}

// ---------- the emission the mounted router produces ----------

/// The observation layer the mounted router emits through.
fn layer() -> Arc<Observability> {
    Observability::shared()
}

/// Enable the writer capture, forget what it holds and start the drain task
/// the gear's initialization would have started.
///
/// The writer holds one receiving half, so the drain is started once for the
/// whole binary and every capture test reads the same ring, filtering on the
/// alias its own request resolved.
fn start_capture() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let shared = layer();
        let _ring = shared.writer().enable_capture();
        // The drain runs on a thread of its own, the way the gear's lifecycle
        // runs it: the runtime of one test shutting down must not take the
        // drain the tests that follow still offer to with it.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the drain runtime builds");
            runtime.block_on(async {
                let token = CancellationToken::new();
                let _ = shared.spawn_drain(token.clone());
                token.cancelled().await;
            });
        });
    });
}

/// Wait for the first captured line `matches` accepts, and return it with its
/// parsed document.
///
/// The lines reach the capture ring when the request path offers them, so the
/// read is a poll rather than a synchronous read: the offer is non-blocking
/// and the line is written to stdout by another task.
async fn wait_for_line(matches: impl Fn(&Value) -> bool) -> (String, Value) {
    let started = Instant::now();
    loop {
        for line in layer().writer().captured() {
            if let Ok(document) = serde_json::from_str::<Value>(&line)
                && matches(&document)
            {
                return (line, document);
            }
        }
        assert!(
            started.elapsed() < EMISSION_TIMEOUT,
            "no line the predicate accepts was emitted: {:?} written={} drops={}",
            layer().writer().captured(),
            layer().writer().written(),
            layer().writer().drops(),
        );
        // The offer never blocks and the drain writes on its own task, so the
        // read yields to the runtime the drain is scheduled on.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Assert the fourteen keys of the audit contract appear on the raw line in
/// their recorded order, and that no other key is on it.
fn assert_key_order(line: &str) {
    let mut previous = 0;
    for key in AUDIT_KEYS {
        let needle = format!("\"{key}\":");
        let at = line
            .find(&needle)
            .unwrap_or_else(|| panic!("the line carries no `{key}` member: {line}"));
        assert!(
            at >= previous,
            "`{key}` is out of the recorded order: {line}"
        );
        previous = at + needle.len();
    }
    let members = line
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split("\",\"")
        .count();
    let mut carried = 0;
    let mut rest = line;
    while let Some(at) = rest.find("\",\"") {
        carried += 1;
        rest = &rest[at + 3..];
    }
    assert_eq!(
        carried + 1,
        members,
        "the member count of the line is the one the raw split reports: {line}"
    );
    for key in AUDIT_KEYS {
        let needle = format!("\"{key}\":");
        assert_eq!(
            line.matches(&needle).count(),
            1,
            "`{key}` appears exactly once: {line}"
        );
    }
}

#[tokio::test]
async fn a_proxied_request_emits_one_line_with_the_fourteen_keys_in_order() {
    start_capture();
    let server = MockServer::start_async().await;
    let alias = alias_for("line");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server, &alias).await;

    // The fixed rate samples one successful line in a hundred, and the counter
    // it advances is the process-wide one, so three times the rate is driven to
    // guarantee the boundary falls inside this test whatever phase the counter
    // is in when the test starts: the sampled line is read from the same writer
    // the failure lines are.
    for _ in 0..(3 * SUCCESS_SAMPLE_RATE) {
        let response = call(
            router.clone(),
            "GET",
            &format!("{path}/things?page=1"),
            Some(context(TENANT, ALL)),
            &[("accept", "application/json"), ("x-oagw-secret", "hunter2")],
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert!(mock.calls_async().await >= SUCCESS_SAMPLE_RATE as usize);
    assert!(
        layer().sampler().success_seen() >= SUCCESS_SAMPLE_RATE,
        "every closed request was classified"
    );
    assert!(
        layer().sampler().emitted_of_success() >= 1,
        "one successful line in a hundred is emitted"
    );

    let (line, document) = wait_for_line(|document| {
        document["event"] == json!(AUDIT_EVENT)
            && document["host"] == json!(alias)
            && document["method"] == json!("GET")
            && document["status"] == json!(200)
            && document["path"] == json!("/v1/things")
    })
    .await;

    // The keys in the recorded order, and no member the contract does not name.
    assert_key_order(&line);
    assert_eq!(document["level"], json!("INFO"), "{line}");
    assert_eq!(document["host"], json!(alias), "{line}");
    assert_eq!(document["path"], json!("/v1/things"), "{line}");
    assert_eq!(document["method"], json!("GET"), "{line}");
    assert_eq!(document["status"], json!(200), "{line}");
    assert_eq!(document["error_type"], Value::Null, "{line}");
    assert!(
        document.get(KEY_ERROR_MESSAGE).is_none(),
        "a success carries no error message: {line}"
    );
    assert!(
        document["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "the correlation identifier is on the line: {line}"
    );
    assert!(document["duration_ms"].is_u64(), "{line}");
    assert_eq!(document["request_size"], json!(0), "{line}");
    assert_eq!(
        document["response_size"],
        json!(r#"{"id":42}"#.len() as u64),
        "the relayed body is counted"
    );
    assert!(document["tenant_id"].is_string(), "{line}");
    assert!(document["principal_id"].is_string(), "{line}");

    // The redaction of the line: no query string, no body, no header value and
    // no credential material is on it, at the success level as anywhere else.
    assert!(!line.contains("?page=1"), "no query string: {line}");
    assert!(!line.contains("application/json"), "no header value: {line}");
    assert!(!line.contains("accept"), "no header name: {line}");
    assert!(!line.contains("hunter2"), "no credential material: {line}");
    assert!(!line.contains("x-oagw-secret"), "no header name: {line}");
    assert!(!line.contains("id\":42"), "no body byte: {line}");
    assert!(
        !line.contains("127.0.0.1"),
        "the endpoint host is not a label of the line: {line}"
    );
    assert!(
        !line.contains("stub.internal"),
        "no other test's alias is on the line: {line}"
    );
}

#[tokio::test]
async fn a_gateway_failure_line_names_the_gts_type_and_agrees_with_the_problem_body() {
    start_capture();
    let server = MockServer::start_async().await;
    let router = mounted_with(&proxy_config(5));
    // The upstream is stored and reachable but declares no route, so the
    // request is refused by the route match and never reaches the upstream.
    let alias = alias_for("reject");
    create_upstream(&router, TENANT, stub_upstream(Some(&alias), server.port())).await;
    let uri = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");

    let response = proxy(router, "GET", &uri).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let problem = json_body(response).await;
    let trace_id = problem["trace_id"].as_str().expect("trace_id").to_owned();

    let (line, document) = wait_for_line(|document| {
        document["event"] == json!(AUDIT_EVENT)
            && document["host"] == json!(alias)
            && document["status"] == json!(404)
            && document["method"] == json!("GET")
    })
    .await;

    assert_key_order(&line);
    assert_eq!(document["level"], json!("WARN"), "{line}");
    assert_eq!(
        document["error_type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"),
        "the GTS type identifier the mapping produced: {line}"
    );
    assert_eq!(
        document["request_id"], json!(trace_id),
        "the line and the problem body carry the same correlation identifier: {line}"
    );
    let message = document[KEY_ERROR_MESSAGE]
        .as_str()
        .expect("a failure carries a redacted detail")
        .to_owned();
    // The detail the client received keeps the route path it matched on; the
    // line records the same detail with the interpolated value removed, so no
    // request-derived value reaches the log store.
    assert!(problem["detail"].as_str().is_some_and(|detail| detail.contains(ROUTE_PATH)));
    assert_eq!(message, "no route of the upstream matches <redacted>", "{line}");
    assert!(
        !line.contains("127.0.0.1"),
        "an upstream that was never dialed is not on the line: {line}"
    );
    assert!(!line.contains("hunter2"), "no credential material: {line}");

    // A request whose alias no tenant holds resolved no upstream at all, so the
    // `host` member is JSON null and never the alias the path carried.
    let absent = alias_for("absent");
    let response = call(
        mounted_with(&proxy_config(5)),
        "GET",
        &format!("{MOUNT_ROOT}/proxy/{absent}{ROUTE_PATH}"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let (line, document) = wait_for_line(|document| {
        document["event"] == json!(AUDIT_EVENT)
            && document["status"] == json!(404)
            && document["host"].is_null()
            && document["path"] == json!(ROUTE_PATH)
    })
    .await;
    assert_eq!(document["host"], Value::Null, "{line}");
    assert_eq!(
        document["path"], json!(ROUTE_PATH),
        "the proxied path, without the mount the alias sat on: {line}"
    );
    // The alias the caller addressed is its own input and may name itself in
    // the detail; what the line must never do is present it as a resolved
    // upstream.
    assert_ne!(document["host"], json!(absent), "{line}");
    assert_ne!(document["host"], json!(alias), "{line}");
}

#[tokio::test]
async fn the_request_families_record_the_closed_request() {
    let server = MockServer::start_async().await;
    let alias = alias_for("fam");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server, &alias).await;
    let metrics = Arc::clone(layer().metrics());
    let requests = |status: &'static str| {
        (
            REQUESTS_TOTAL,
            vec![
                (LABEL_HOST.to_owned(), alias.clone()),
                (LABEL_METHOD.to_owned(), "GET".to_owned()),
                (LABEL_ROUTE.to_owned(), ROUTE_PATH.to_owned()),
                (LABEL_STATUS.to_owned(), status.to_owned()),
            ],
        )
    };

    // The series is this test's own, so the delta it reads is the one request
    // it drove: no other test writes to the same label set.
    let before = metrics.counter(requests("200").0, &labels(&requests("200").1));
    let gateway_before = metrics.histogram(
        REQUEST_DURATION,
        &[
            (LABEL_HOST, alias.as_str()),
            (LABEL_ROUTE, ROUTE_PATH),
            (LABEL_PHASE, PHASE_GATEWAY_ADDED),
        ],
    );

    let response = call(
        router.clone(),
        "GET",
        &format!("{path}/things"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls_async().await, 1);

    let after = metrics
        .counter(requests("200").0, &labels(&requests("200").1))
        .expect("the request counter series exists");
    assert_eq!(after, before.unwrap_or(0) + 1, "one request was counted");
    let gateway_after = metrics
        .histogram(
            REQUEST_DURATION,
            &[
                (LABEL_HOST, alias.as_str()),
                (LABEL_ROUTE, ROUTE_PATH),
                (LABEL_PHASE, PHASE_GATEWAY_ADDED),
            ],
        )
        .expect("the gateway-added series exists");
    assert_eq!(
        gateway_after.count,
        gateway_before.as_ref().map_or(0, |series| series.count) + 1,
        "one gateway-added observation per closed request"
    );
    assert_eq!(
        gateway_after.buckets.len(),
        DURATION_BUCKETS.len(),
        "the twelve buckets"
    );
    assert_eq!(gateway_after.buckets[11], gateway_after.count, "cumulative");
    assert!(gateway_after.sum > 0.0, "the duration is observed");
    assert!(
        metrics
            .histogram(
                REQUEST_DURATION,
                &[
                    (LABEL_HOST, alias.as_str()),
                    (LABEL_ROUTE, ROUTE_PATH),
                    (LABEL_PHASE, PHASE_UPSTREAM),
                ],
            )
            .is_some(),
        "the upstream call is observed in its own phase"
    );

    // A gateway rejection increments the error family with the GTS type the
    // audit line carries for the same request, against the route the request
    // matched: a path that matches no route is labelled with the recorded
    // fallback, never with the raw path.
    let route_not_found = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    let rejected = [
        (LABEL_HOST, alias.as_str()),
        (LABEL_ROUTE, LABEL_OTHER),
        (LABEL_ERROR_TYPE, route_not_found),
    ];
    let error_before = metrics.counter(ERRORS_TOTAL, &rejected);
    let response = proxy(router, "GET", &format!("{MOUNT_ROOT}/proxy/{alias}/no-route")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error_after = metrics
        .counter(ERRORS_TOTAL, &rejected)
        .expect("the error series exists");
    assert_eq!(
        error_after,
        error_before.unwrap_or(0) + 1,
        "the gateway rejection is counted"
    );
    assert_eq!(
        metrics.counter(requests("404").0, &labels(&requests("404").1)),
        None,
        "the rejected request carried no upstream status of its own"
    );
    let rejected_request = (
        REQUESTS_TOTAL,
        vec![
            (LABEL_HOST.to_owned(), alias.clone()),
            (LABEL_METHOD.to_owned(), "GET".to_owned()),
            (LABEL_ROUTE.to_owned(), LABEL_OTHER.to_owned()),
            (LABEL_STATUS.to_owned(), "404".to_owned()),
        ],
    );
    assert_eq!(
        metrics.counter(rejected_request.0, &labels(&rejected_request.1)),
        Some(1),
        "the rejection is counted against the status the client received"
    );
}

#[tokio::test]
async fn the_routing_series_and_the_in_flight_series_record_the_selection() {
    let server = MockServer::start_async().await;
    let alias = alias_for("route");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server, &alias).await;
    let metrics = Arc::clone(layer().metrics());
    let upstream = only_upstream_id(&router).await.to_string();

    let response = call(
        router,
        "GET",
        &format!("{path}/things"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls_async().await, 1);

    // The endpoint host label is the stored endpoint identifier, which carries
    // the port the record holds, so the selection is summed over the endpoints
    // the alias reports.
    let pairs: Vec<(String, String)> = metrics
        .endpoint_pairs()
        .into_iter()
        .filter(|(host, _)| host == &alias)
        .collect();
    assert!(
        pairs
            .iter()
            .any(|(_, endpoint)| endpoint == "127.0.0.1"),
        "the endpoint pair of the selection is reported: {pairs:?}"
    );

    let mut selected = 0;
    for (_, endpoint) in &pairs {
        selected += metrics
            .counter(
                ROUTING_ENDPOINT_SELECTED,
                &[
                    (LABEL_UPSTREAM_ID, upstream.as_str()),
                    (LABEL_ENDPOINT_HOST, endpoint.as_str()),
                    (LABEL_SELECTION_METHOD, "default"),
                ],
            )
            .unwrap_or(0);
    }
    assert_eq!(
        selected, 1,
        "the one selection the test drove was counted: {selected}"
    );

    // The health and the connection gauges read the live state at scrape time,
    // so the values the scrape reports are read here through the same source
    // the registered callback reads them from.
    for (_, endpoint) in &pairs {
        assert!(
            metrics
                .breaker_state_of(endpoint)
                .is_none_or(|state| state.as_code() == 0),
            "a reachable endpoint whose breaker is closed is up"
        );
        assert_eq!(
            metrics
                .in_flight_by_host()
                .get(&alias)
                .copied()
                .unwrap_or_default(),
            0,
            "the exchange the request opened is closed again"
        );
    }

    // The context the request opened is closed by the request's own exit, so
    // no series leaks an increment. The gauge is read per upstream alias: the
    // binary's tests share the process-wide registry, so the pending total of
    // the whole process is not a value this test owns.
    assert!(
        metrics
            .in_flight_by_host()
            .get(&alias)
            .is_none_or(|value| *value == 0),
        "the per-host in-flight gauge returned to zero"
    );
}

#[tokio::test]
async fn the_breaker_series_are_recorded_from_the_breaker_state() {
    let server = MockServer::start_async().await;
    let alias = alias_for("break");
    stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server, &alias).await;
    let metrics = Arc::clone(layer().metrics());

    // The endpoint is noted by the closed request, so a breaker that never left
    // `closed` is distinguishable from an absent one.
    let response = call(
        router,
        "GET",
        &format!("{path}/things"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let hosts: Vec<String> = metrics
        .breaker_hosts()
        .into_iter()
        .filter(|host| host == "127.0.0.1")
        .collect();
    assert!(
        !hosts.is_empty(),
        "the breaker of the selected endpoint is reported: {hosts:?}"
    );
    for host in &hosts {
        assert_eq!(
            metrics
                .breaker_state_of(host)
                .map(|state| state.as_code())
                .expect("every reported breaker carries a state"),
            0,
            "a breaker that never opened is `closed`"
        );
        assert!(
            metrics
                .counter(
                    BREAKER_TRANSITIONS,
                    &[
                        (LABEL_HOST, host.as_str()),
                        (LABEL_FROM_STATE, STATE_CLOSED),
                        (LABEL_TO_STATE, STATE_CLOSED),
                    ],
                )
                .is_none(),
            "a breaker that never opened reports no transition series"
        );
    }
}

/// The live state of one real breaker, as the engine's adapter hands it to the
/// registry the scrape reads.
struct BreakerFeed {
    breaker: Arc<CircuitBreaker>,
}

impl RuntimeState for BreakerFeed {
    fn breaker_state(&self, host: &str) -> BreakerState {
        self.breaker.state(host)
    }

    fn breaker_drain_transitions(&self) -> Vec<Transition> {
        self.breaker.drain_transitions()
    }
}

/// The transition counter counts the edge the breaker took on the admission of
/// the probe, and not only the ones its outcomes produce: a trip, the probe the
/// cool-down admits and the outcome of that probe are three series of the one
/// family, and each is counted once, when a request closes.
#[tokio::test]
async fn the_breaker_transition_counter_counts_the_admission_of_the_probe() {
    // The endpoint host the breaker watches, as the transition labels carry it.
    let host = "127.0.0.1:9";
    // A cool-down no test waits out for thirty seconds: fifty milliseconds is
    // what the probe needs to be admissible inside this test.
    let breaker = Arc::new(CircuitBreaker::with_windows(
        Duration::from_millis(50),
        Duration::from_secs(5),
    ));
    let metrics = Arc::new(MetricRegistry::new());
    metrics
        .register()
        .expect("the families register on a fresh registry");
    metrics.attach_state_source(Arc::new(BreakerFeed {
        breaker: Arc::clone(&breaker),
    }));
    let mut context = RequestContext::new(
        "corr-breaker".to_owned(),
        format!("/{host}/v1/things"),
        "GET".to_owned(),
    );
    context.alias = Some(host.to_owned());
    let closed = || {
        metrics.record_request(&RequestObservation {
            context: &context,
            status: 503,
            upstream_called: false,
            duration: Duration::from_millis(1),
            response_bytes: None,
            tenant_id: None,
            principal_id: None,
            error: None,
        });
    };
    let series = |from: &'static str, to: &'static str| {
        [
            (LABEL_HOST, host),
            (LABEL_FROM_STATE, from),
            (LABEL_TO_STATE, to),
        ]
    };

    for _ in 0..FAILURE_THRESHOLD {
        breaker.record(host, CallOutcome::Failure);
    }
    assert_eq!(breaker.state(host), BreakerState::Open);
    closed();
    assert_eq!(
        metrics.counter(BREAKER_TRANSITIONS, &series(STATE_CLOSED, STATE_OPEN)),
        Some(1),
        "the trip the window produced is counted"
    );

    // The cool-down elapsed, so the next admission is the probe: the edge the
    // admission took is the one the counter counts, before any outcome exists.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        breaker.admit(host).is_ok(),
        "the cooled breaker admits the probe"
    );
    assert_eq!(breaker.state(host), BreakerState::HalfOpen);
    closed();
    assert_eq!(
        metrics.counter(BREAKER_TRANSITIONS, &series(STATE_OPEN, STATE_HALF_OPEN)),
        Some(1),
        "the probe's admission is counted"
    );
    assert!(
        metrics
            .counter(BREAKER_TRANSITIONS, &series(STATE_HALF_OPEN, STATE_CLOSED))
            .is_none(),
        "no outcome has moved the probe yet"
    );

    breaker.record(host, CallOutcome::Success);
    assert_eq!(breaker.state(host), BreakerState::Closed);
    closed();
    assert_eq!(
        metrics.counter(BREAKER_TRANSITIONS, &series(STATE_HALF_OPEN, STATE_CLOSED)),
        Some(1),
        "the probe's outcome is counted"
    );
    // The drain handed every transition over exactly once, so a further closed
    // request counts nothing new.
    closed();
    assert_eq!(
        metrics.counter(BREAKER_TRANSITIONS, &series(STATE_HALF_OPEN, STATE_CLOSED)),
        Some(1),
        "the drain hands each transition over once"
    );
}

#[tokio::test]
async fn a_preflight_is_answered_without_emitting_a_line() {
    start_capture();
    let router = mounted_with(&proxy_config(5));

    let response = call(
        router,
        "OPTIONS",
        &format!("{MOUNT_ROOT}/proxy/{ALIAS}{ROUTE_PATH}/things"),
        None,
        &[
            ("origin", "https://app.dev"),
            ("access-control-request-method", "POST"),
        ],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let answer = text(response).await;
    assert!(answer.is_empty(), "a preflight answer carries no body");

    // The preflight left the handler before the exchange opened, so nothing is
    // emitted for it: the read yields to the drain task before it asserts.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for line in layer().writer().captured() {
        let document: Value = serde_json::from_str(&line).expect("the line is a JSON document");
        assert_ne!(
            document["method"], json!("OPTIONS"),
            "a preflight produces no line: {line}"
        );
    }
}

#[tokio::test]
async fn an_ancestor_route_is_recorded_against_the_resolved_alias() {
    let server = MockServer::start_async().await;
    let alias = alias_for("legacy");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    // The ancestor holds the alias; the calling tenant inherits it.
    let chain = BTreeMap::from([(TENANT, vec![ANCESTOR])]);
    let router = mounted_over(&proxy_config(5), Arc::new(StaticHierarchy::new(chain)));
    let stored = create_upstream(&router, ANCESTOR, stub_upstream(Some(&alias), server.port())).await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, ANCESTOR, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");

    let response = call(
        router,
        "GET",
        &format!("{path}/things"),
        Some(context(TENANT, ALL)),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls_async().await, 1);
    assert_eq!(
        layer().metrics().counter(
            REQUESTS_TOTAL,
            &[
                (LABEL_HOST, alias.as_str()),
                (LABEL_METHOD, "GET"),
                (LABEL_ROUTE, ROUTE_PATH),
                (LABEL_STATUS, "200"),
            ],
        ),
        Some(1),
        "the inherited route is recorded under the resolved alias"
    );
}

/// The sampling decision is taken on the record alone and is uniform over the
/// routes, so a high-volume route is sampled at the same fixed rate as any
/// other: the counter the integration test drove past its rate is read back
/// here, against the sampling arithmetic of [`Sampler`].
#[tokio::test]
async fn the_sampled_lines_are_the_fixed_rate_of_the_classified_successes() {
    let shared = layer();
    let sampler = shared.sampler();
    let seen = sampler.success_seen();
    let emitted = sampler.emitted_of_success();
    let dropped = sampler.sampled();
    assert_eq!(sampler.success_seen(), seen, "the counter only grows");
    assert_eq!(
        emitted,
        seen / SUCCESS_SAMPLE_RATE,
        "the emitted count is the fixed rate of the successes seen"
    );
    assert_eq!(emitted + dropped, seen, "no classified line is unaccounted");
    assert_eq!(
        SUCCESS_SAMPLE_RATE, 100,
        "the rate is a constant of the module and not a configuration key"
    );
    assert!(
        emitted * SUCCESS_SAMPLE_RATE <= seen,
        "the fixed rate never emits more than one line in a hundred"
    );
}

// ---------- the classes the line and the families both have to cover ----------

/// An exhausted bucket is recorded by both rate-limit families, under the route
/// the request matched and never under the raw request path, and the `429` is
/// one emitted line of its own.
#[tokio::test]
async fn the_rate_limit_families_record_the_exhausted_bucket() {
    start_capture();
    let server = MockServer::start_async().await;
    let alias = alias_for("rl");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with_chains(&proxy_config(5));
    let stored = create_upstream(&router, TENANT, {
        let mut body = stub_upstream(Some(&alias), server.port());
        body["rate_limit"] = json!({
            "sustained": { "rate": 1, "window": "second" },
            "burst": { "capacity": 1 },
            "scope": "tenant",
            "strategy": "reject"
        });
        body
    })
    .await;
    let upstream = Uuid::parse_str(stored["id"].as_str().expect("id")).expect("uuid");
    create_route(&router, TENANT, route_body(upstream, ROUTE_PATH)).await;
    let path = format!("{MOUNT_ROOT}/proxy/{alias}{ROUTE_PATH}");
    let metrics = Arc::clone(layer().metrics());
    let exceeded = [
        (LABEL_HOST, alias.as_str()),
        (LABEL_PATH, ROUTE_PATH),
    ];
    let ratio = [
        (LABEL_HOST, alias.as_str()),
        (LABEL_PATH, ROUTE_PATH),
    ];

    let first = proxy(router.clone(), "GET", &format!("{path}/things")).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", text(first).await);
    assert_eq!(mock.calls_async().await, 1);
    let second = proxy(router, "GET", &format!("{path}/things")).await;
    let limit = second
        .headers()
        .get("x-ratelimit-limit")
        .map(|value| value.to_str().expect("ascii").to_owned());
    let remaining = second
        .headers()
        .get("x-ratelimit-remaining")
        .map(|value| value.to_str().expect("ascii").to_owned());
    let status = second.status();
    let body = text(second).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "limit={limit:?} remaining={remaining:?} body={body}"
    );
    assert_eq!(mock.calls_async().await, 1, "the throttled request stayed");

    let counted = metrics
        .counter(RATE_LIMIT_EXCEEDED, &exceeded)
        .expect("the exceeded series exists");
    assert_eq!(counted, 1, "the one rejected request is counted");
    let usage = metrics
        .gauge(RATE_LIMIT_USAGE_RATIO, &ratio)
        .expect("the usage series exists");
    assert!(
        (usage - 1.0).abs() < f64::EPSILON,
        "an exhausted bucket is reported at the top of its range: {usage}"
    );

    let (line, document) = wait_for_line(|line| line["status"] == json!(429)).await;
    assert_eq!(document["event"], json!(AUDIT_EVENT), "{line}");
    assert_eq!(document["host"], json!(alias), "{line}");
    // The line's `path` is the forwarded request path; the family's `path`
    // label is the route's configured match path, which the series above
    // asserts on.
    assert_eq!(document["path"], json!("/v1/things"), "{line}");
    assert_eq!(document["error_type"], json!("gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"), "{line}");
    assert_key_order(&line);
}

/// A request the transport refuses before the pipeline — unauthenticated and
/// over the declared body bound — is still one emitted line and one request
/// series update, at the status the client received.
#[tokio::test]
async fn a_transport_refused_request_is_still_one_emitted_line() {
    start_capture();
    let server = MockServer::start_async().await;
    let alias = alias_for("gate");
    let mock = stub_ok(&server, "/v1/things", r#"{"id":42}"#).await;
    let router = mounted_with(&proxy_config(5));
    let path = stubbed(&router, &server, &alias).await;
    let metrics = Arc::clone(layer().metrics());

    // The proxy endpoint without an authenticated caller.
    let response = call(router.clone(), "GET", &format!("{path}/things"), None, &[], None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (line, document) = wait_for_line(|line| line["status"] == json!(401)).await;
    assert_eq!(document["event"], json!(AUDIT_EVENT), "{line}");
    // The refusal closed the context the transport opened, before the walk
    // resolved the alias: the line carries the forwarded path and no host.
    assert_eq!(document["host"], json!(null), "{line}");
    assert_eq!(document["path"], json!("/v1/things"), "{line}");
    assert_eq!(
        document["error_type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"),
        "{line}"
    );
    // No body was read for a refusal the gate took, so the line carries no
    // request size.
    assert_eq!(document["request_size"], json!(null), "{line}");
    assert_key_order(&line);
    assert_eq!(
        metrics.counter(
            REQUESTS_TOTAL,
            &[
                (LABEL_HOST, LABEL_OTHER),
                (LABEL_METHOD, "GET"),
                (LABEL_ROUTE, LABEL_OTHER),
                (LABEL_STATUS, "401"),
            ],
        ),
        Some(1),
        "the refusal is one counted proxied request, labelled with the          fallbacks the unresolved facts fall back to"
    );

    // The declared body over the hard limit, refused before a byte is read.
    let declared = format!("{}", oagw::infra::proxy::validate::BODY_HARD_LIMIT + 1);
    let response = call(
        router,
        "POST",
        &format!("{path}/things"),
        Some(context(TENANT, ALL)),
        &[("content-length", declared.as_str())],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let (line, document) = wait_for_line(|line| line["status"] == json!(413)).await;
    assert_eq!(document["host"], json!(null), "{line}");
    assert_eq!(document["path"], json!("/v1/things"), "{line}");
    assert_eq!(
        document["error_type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"),
        "{line}"
    );
    assert_key_order(&line);
    assert_eq!(
        metrics.counter(
            REQUESTS_TOTAL,
            &[
                (LABEL_HOST, LABEL_OTHER),
                (LABEL_METHOD, "POST"),
                (LABEL_ROUTE, LABEL_OTHER),
                (LABEL_STATUS, "413"),
            ],
        ),
        Some(1),
        "the oversized request is one counted proxied request"
    );
    assert_eq!(mock.calls_async().await, 0, "neither refusal reached upstream");
}
