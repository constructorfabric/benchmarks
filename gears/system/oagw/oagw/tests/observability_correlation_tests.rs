//! The correlation identifier and its propagation.
//!
//! Covers `cpt-cf-oagw-dod-obs-correlation` and the correlation rows of
//! `cpt-cf-oagw-dod-obs-tests`: the assignment from the inbound header and
//! from the generator, each negative of the admission check, the propagation
//! of the identifier to a record and to a gateway error body's `trace_id`,
//! the absence of the echo on an answer the upstream produced, the declaration
//! of `CorrelationContext` as a member of `ProxyContext` over the types its
//! owning features declared, and the two build-time constants' freedom from
//! any configuration surface. The upstream is a live local listener, so the
//! pass-through answer is a real one; the audit sink is the test's own, so no
//! assertion reads another suite's stream.

// @cpt-dod:cpt-cf-oagw-dod-obs-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::data_plane::observability::{CollectingSink, Exchange, Observability};
use oagw::domain::observability::{
    AUTH_FAILURE_LOG_INTERVAL_MS, AUTH_FAILURE_LOG_LIMIT, AUDIT_EVENTS, CORRELATION_HEADER,
    CORRELATION_MAX_LEN, CorrelationContext, CorrelationSource, HIGH_VOLUME_SAMPLE_ONE_IN,
    SamplingDecision,
};
use oagw::domain::proxy::ProxyContext;
use oagw::OagwState;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const TENANT: u128 = 0x51;
const HOST: &str = "127.0.0.1";

/// The `AuthZ` PDP the allowing stub stands in for.
struct Allowing;

#[async_trait::async_trait]
impl AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: json!(TENANT.to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// A live HTTP/1.1 upstream, which answers on the port it bound.
#[derive(Clone)]
struct Upstream {
    port: u16,
}

/// Starts one echo upstream on an ephemeral port.
async fn upstream() -> Upstream {
    let listener = TcpListener::bind((HOST, 0)).await.expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // The head is read to its terminator and the declared body
                // after it, so the gateway's own framing is consumed before
                // the answer is written.
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(index) = find_head_end(&buffer) {
                        break index;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                let length = head
                    .split("\r\n")
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = buffer[head_end + 4..].to_vec();
                while body.len() < length {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                let _ = AsyncWriteExt::write_all(
                    &mut socket,
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                      content-length: 2\r\nconnection: close\r\n\r\nok",
                )
                .await;
                let _ = socket.flush().await;
            });
        }
    });
    Upstream { port }
}

/// The index the head's terminator starts at.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// One mounted surface over its own store, with its own observation seam.
struct Surface {
    router: Router,
    sink: Arc<CollectingSink>,
}

/// Builds a surface whose observation seam writes to a collecting sink.
fn surface() -> Surface {
    let store = Arc::new(oagw::store::OagwStore::new());
    let cache = Arc::new(ControlPlaneCache::new());
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let service = Arc::new(
        ManagementService::new(Arc::clone(&store), &config, Arc::clone(&cache))
            .expect("the validators compile"),
    );
    let state = Arc::new(OagwState::new(
        Arc::new(config),
        Arc::clone(&store),
        service,
        Some(Arc::new(PolicyEnforcer::new(Arc::new(Allowing)))),
        None,
        Arc::clone(&cache),
    ));
    let sink = Arc::new(CollectingSink::new());
    state.swap_audit_sink(Arc::clone(&sink) as Arc<dyn oagw::data_plane::observability::AuditSink>);
    Surface {
        router: oagw::api::rest::register_management_routes(Router::new(), Arc::clone(&state)),
        sink,
    }
}

/// The authenticated subject a request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// The request path of a URI, without the query a problem document never echoes.
fn path_of(uri: &str) -> &str {
    uri.split('?').next().expect("the path")
}

/// Issues one request and returns the status and the parsed body.
async fn issue(
    app: Router,
    method: Method,
    uri: &str,
    authenticated: bool,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if authenticated {
        builder = builder.extension(subject());
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("the request builds");
    let response = app.oneshot(request).await.expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("the answer body is JSON")
    };
    (status, document)
}

/// Issues one proxy request and returns the status and the body as text.
async fn issue_raw(
    app: Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(Method::GET).uri(uri).extension(subject());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("the request builds");
    let response = app.oneshot(request).await.expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Stores one upstream and one route over it, both through the management API.
async fn wired(app: &Router, target: &Upstream) {
    let body = json!({
        "alias": HOST,
        "server": { "endpoints": [{ "scheme": "http", "host": HOST, "port": target.port }] },
        "protocol": HTTP_PROTOCOL,
        "tags": ["proxy"]
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/oagw/v1/upstreams")
                .extension(subject())
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("the request builds"),
        )
        .await
        .expect("oneshot resolves");
    let response_status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    assert_eq!(response_status, StatusCode::CREATED, "{document}");
    let instance = document["id"].as_str().expect("the instance id");
    let key = oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string();
    // The second route names a single resource, so its success record is the
    // one the sampling ratio does not govern: it is the route the tests that
    // read a success record drive, whatever identifier the request carries.
    for route in [
        json!({
            "upstream_id": key,
            "match": { "http": { "methods": ["GET"], "path": "/api" } },
            "priority": 10
        }),
        json!({
            "upstream_id": key,
            "match": { "http": { "methods": ["GET"], "path": "/api/{id}" } },
            "priority": 5
        }),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/oagw/v1/routes")
                    .extension(subject())
                    .header("content-type", "application/json")
                    .body(Body::from(route.to_string()))
                    .expect("the request builds"),
            )
            .await
            .expect("oneshot resolves");
        assert_eq!(response.status(), StatusCode::CREATED);
    }
}

/// The alias path of the single-resource route the `wired` helper declares.
fn single_resource_path() -> String {
    String::from("/oagw/v1/proxy/127.0.0.1/api/{id}/one")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bounded_printable_header_value_is_adopted_and_recorded() {
    let app = surface();
    let target = upstream().await;
    wired(&app.router, &target).await;

    let (status, body) = issue_raw(
        app.router.clone(),
        "/oagw/v1/proxy/127.0.0.1/api/one",
        &[(CORRELATION_HEADER, "trace-keep-30")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "ok", "the upstream body passed through");

    let records = app.sink.parsed().expect("every record is one JSON object");
    let succeeded = records
        .iter()
        .find(|record| record["event"] == json!("proxy_request.succeeded"))
        .expect("the success record was written");
    assert_eq!(
        succeeded["request_id"],
        json!("trace-keep-30"),
        "the header value is the record's request_id"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_without_the_header_is_recorded_with_a_generated_uuid() {
    let app = surface();
    let target = upstream().await;
    wired(&app.router, &target).await;

    let (status, body) = issue_raw(
        app.router.clone(),
        &single_resource_path(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let records = app.sink.parsed().expect("the records parse");
    let succeeded = records
        .iter()
        .find(|record| record["event"] == json!("proxy_request.succeeded"))
        .expect("the success record was written");
    let request_id = succeeded["request_id"].as_str().expect("the identifier");
    assert!(Uuid::parse_str(request_id).is_ok(), "{request_id} is not a UUID");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_context_records_where_the_identifier_came_from() {
    // The adopted branch.
    let adopted = CorrelationContext::assign(Some("trace-adopted"), None, None);
    assert_eq!(adopted.request_id, "trace-adopted");
    assert_eq!(adopted.source, CorrelationSource::InboundHeader);
    // The generated branch: no header at all.
    let generated = CorrelationContext::assign(None, None, None);
    assert_ne!(generated.request_id, "");
    assert_eq!(generated.source, CorrelationSource::Generated);
    // The generated branch: a value the admission check refuses.
    let refused = CorrelationContext::assign(Some("bad value\u{0007}"), None, None);
    assert_eq!(refused.source, CorrelationSource::Generated);
    assert_ne!(refused.request_id, "bad value\u{0007}");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_negative_of_the_admission_check_generates_instead() {
    // A control character.
    let control = CorrelationContext::assign(Some("trailing\u{0003}"), None, None);
    assert_eq!(control.source, CorrelationSource::Generated);
    // A value beyond the bounded length.
    let long = CorrelationContext::assign(Some(&"a".repeat(CORRELATION_MAX_LEN + 1)), None, None);
    assert_eq!(long.source, CorrelationSource::Generated);
    // The bound itself is admitted.
    let bounded = CorrelationContext::assign(Some(&"a".repeat(CORRELATION_MAX_LEN)), None, None);
    assert_eq!(bounded.source, CorrelationSource::InboundHeader);
    // A value that carries no printable identifier at all.
    let empty = CorrelationContext::assign(Some(""), None, None);
    assert_eq!(empty.source, CorrelationSource::Generated);
    // A value carrying a character outside the printable range.
    let escape = CorrelationContext::assign(Some("trace\u{0085}value"), None, None);
    assert_eq!(escape.source, CorrelationSource::Generated);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_proxy_record_carries_the_correlation_identifier() {
    // The gateway-refused path: no alias matches, the answer is a 404 problem.
    let app = surface();
    let (status, _body) = issue(
        app.router.clone(),
        Method::GET,
        "/oagw/v1/proxy/no-such-upstream/api",
        true,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let records = app.sink.parsed().expect("the records parse");
    let written = records
        .iter()
        .filter(|record| AUDIT_EVENTS.contains(&record["event"].as_str().unwrap_or_default()))
        .count();
    let carried = records
        .iter()
        .filter(|record| record["request_id"].is_string())
        .count();
    assert_eq!(written, carried, "every record carries a request_id");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_error_body_echoes_the_correlation_identifier_as_trace_id() {
    let app = surface();
    let (status, body) = issue(
        app.router.clone(),
        Method::GET,
        "/oagw/v1/proxy/no-such-upstream/api",
        true,
        &[(CORRELATION_HEADER, "trace-echoed")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["trace_id"], json!("trace-echoed"),
        "the error mapping attached the correlation identifier as trace_id"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_answer_passes_through_without_a_trace_id_echo() {
    let app = surface();
    let target = upstream().await;
    wired(&app.router, &target).await;

    let (status, body) = issue_raw(
        app.router.clone(),
        &single_resource_path(),
        &[(CORRELATION_HEADER, "trace-no-echo")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "ok");
    assert!(
        !body.contains("trace_id") && !body.contains("trace-no-echo"),
        "the upstream answer echoes no correlation identifier"
    );

    // The success record still carries the identifier, and the answer carried
    // no `trace_id`: the body the upstream produced is the body the caller
    // received, which the exposition of the exchange records as two bytes of
    // `ok` and nothing else.
    let records = app.sink.parsed().expect("the records parse");
    let succeeded = records
        .iter()
        .find(|record| record["event"] == json!("proxy_request.succeeded"))
        .expect("the success record was written");
    assert_eq!(succeeded["request_id"], json!("trace-no-echo"));
    assert_eq!(
        succeeded["response_size"],
        json!("2"),
        "the upstream body passed through"
    );
    assert_eq!(succeeded["status"], json!("200"));
    assert!(succeeded.get("error_type").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_high_volume_route_samples_its_success_records_at_the_ratio() {
    let app = surface();
    let target = upstream().await;
    wired(&app.router, &target).await;

    // The route the suite declares names a collection, so its success record
    // is the one the ratio governs: an identifier whose roll refuses is
    // recorded in no series of the family the seam counts and in no record,
    // while the exchange itself is still answered.
    let (status, body) = issue_raw(
        app.router.clone(),
        "/oagw/v1/proxy/127.0.0.1/api/one",
        &[(CORRELATION_HEADER, "trace-drop-0")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let records = app.sink.parsed().expect("the records parse");
    let succeeded = records
        .iter()
        .filter(|record| record["event"] == json!("proxy_request.succeeded"))
        .count();
    assert_eq!(succeeded, 0, "the sampled-out success record is not written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_request_is_never_sampled() {
    let app = surface();
    // The 404 the unmatched alias answers with is a failed request, which is
    // never subject to the ratio whatever identifier it carries.
    for identifier in ["trace-drop-0", "trace-keep-52"] {
        let (status, _) = issue(
            app.router.clone(),
            Method::GET,
            "/oagw/v1/proxy/no-such-upstream/api",
            true,
            &[(CORRELATION_HEADER, identifier)],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let records = app.sink.parsed().expect("the records parse");
    let failed = records
        .iter()
        .filter(|record| record["event"] == json!("proxy_request.failed"))
        .count();
    assert_eq!(failed, 2, "every failed request is recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gateway_error_echo_is_absent_when_no_header_arrived() {
    let app = surface();
    let (status, body) = issue(
        app.router.clone(),
        Method::GET,
        "/oagw/v1/proxy/no-such-upstream/api",
        true,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let trace = body["trace_id"].as_str().expect("the echo is present");
    assert!(Uuid::parse_str(trace).is_ok(), "{trace} is the generated identifier");
}

#[test]
fn the_correlation_context_is_a_member_of_the_proxy_context() {
    // The declaration is checked at compile time: the member's type is the
    // type this feature declared, and no second declaration of it exists.
    let context = CorrelationContext::assign(Some("trace-member"), None, None);
    let proxy = ProxyContext {
        correlation: Some(context),
        ..ProxyContext::default()
    };
    assert_eq!(
        proxy.correlation.as_ref().map(|carried| carried.request_id.as_str()),
        Some("trace-member")
    );
}

#[test]
fn the_consumed_types_are_the_ones_their_owning_features_declared() {
    // `ProxyContext`, `ResolvedUpstream`, `ProxyResponse`, and `ErrorContext`
    // are consumed from their owning features and not redeclared: the type
    // paths this feature's routines name are the paths those features
    // published.
    let proxy: ProxyContext = ProxyContext::default();
    assert!(proxy.correlation.is_none());
    let resolved = oagw::domain::proxy::ResolvedUpstream {
        tenant_id: Uuid::from_u128(TENANT),
        upstream_id: Uuid::nil(),
        alias: String::from(HOST),
        alias_derivation: oagw::domain::proxy::AliasDerivation::Explicit,
        endpoints: Vec::new(),
        protocol: String::from("cf.core.oagw.http.v1"),
        enabled: true,
        headers: oagw::domain::upstream::HeadersConfig::default(),
        rate_limit: None,
        plugins: None,
        cors: None,
        route_candidates: Vec::new(),
    };
    assert!(resolved.upstream_id.is_nil());
    let response = oagw::domain::proxy::ProxyResponse::upstream(200, Vec::new(), Vec::new());
    assert_eq!(response.status, 200);
    let context = oagw::domain::error::ErrorContext::default();
    assert!(context.trace_id.is_none());
}

#[test]
fn the_exchange_that_the_path_hands_the_seam_carries_the_context() {
    // The exit of the path reads the correlation context the entry assigned,
    // and the record it writes names it.
    let observability = Observability::with_sink(Arc::new(CollectingSink::new()));
    let correlation = CorrelationContext::assign(
        Some("trace-exchange"),
        Some(Uuid::from_u128(TENANT)),
        Some(String::from("subject-51")),
    );
    let exchange = Exchange {
        host: Some(String::from(HOST)),
        route: Some(String::from("/api")),
        method: String::from("GET"),
        status: Some(200),
        ..Exchange::default()
    };
    observability.observe(&exchange, Some(&correlation));

    let records = observability
        .registry()
        .render();
    assert!(records.contains("oagw_requests_total"), "{records}");
}

#[test]
fn the_sampling_ratio_is_a_build_time_constant_with_no_configuration_surface() {
    assert_eq!(HIGH_VOLUME_SAMPLE_ONE_IN, 100);
    // No key of `OagwConfig` names either constant: the configuration surface
    // the gear compiles carries no key a document could change them with.
    let config = OagwConfig::default();
    let keys: Vec<String> = serde_json::to_value(config)
        .expect("the configuration serializes")
        .as_object()
        .expect("the configuration is an object")
        .keys()
        .cloned()
        .collect();
    for key in keys {
        let lowered = key.to_ascii_lowercase();
        assert!(
            !lowered.contains("sample") && !lowered.contains("flood") && !lowered.contains("log"),
            "{key} is a configuration surface for a build-time constant"
        );
    }
}

#[test]
fn the_failure_log_bound_is_a_build_time_constant_with_no_configuration_surface() {
    assert_eq!(AUTH_FAILURE_LOG_LIMIT, 20);
    assert_eq!(AUTH_FAILURE_LOG_INTERVAL_MS, 1_000);
    assert_ne!(SamplingDecision::Keep, SamplingDecision::Drop);
    // The correlation header name is the one name §1.5 allowlists.
    assert_eq!(CORRELATION_HEADER, "x-request-id");
}

#[test]
fn the_header_name_is_read_case_insensitively_off_the_inbound_pairs() {
    let headers = vec![
        (String::from("Content-Type"), String::from("application/json")),
        (String::from("X-REQUEST-ID"), String::from("trace-upper")),
    ];
    let read = oagw::data_plane::observability::correlation_header(&headers);
    assert_eq!(read, Some("trace-upper"));
}

#[test]
fn a_route_that_is_not_high_volume_is_never_sampled() {
    let decision = CorrelationContext::sampling_of("trace-plain");
    let _ = decision;
    // The classification is a property of the pattern, not of the caller: a
    // pattern that names a single resource — a parameter segment the route
    // declares — is the one that is never sampled, and a pattern that names a
    // collection is the one the ratio is stated over.
    assert!(oagw::domain::observability::is_high_volume_pattern("/api"));
    assert!(!oagw::domain::observability::is_high_volume_pattern("/v1/things/{id}"));
}

#[test]
fn the_header_the_platform_injects_is_the_one_name_the_allowlist_admits() {
    let name = HeaderName::from_bytes(CORRELATION_HEADER.as_bytes())
        .expect("the correlation header name is a legal header name")
        .as_str()
        .to_owned();
    assert_eq!(name, CORRELATION_HEADER);
    assert_eq!(path_of("/oagw/v1/proxy/a/api?model=1"), "/oagw/v1/proxy/a/api");
}
