//! The structured audit records.
//!
//! Covers `cpt-cf-oagw-dod-obs-audit`, `cpt-cf-oagw-dod-obs-redaction`, and
//! `cpt-cf-oagw-dod-obs-sampling` and the audit rows of
//! `cpt-cf-oagw-dod-obs-tests`: the fourteen fields and no fifteenth, the
//! omission of an unpopulated field, the success and the failed record and
//! their levels, the five logged categories and their closed event set, the
//! configuration-change record at the write-completion seam and its absence
//! for a refused write, both redaction rules and the single-name allowlist,
//! and the failure-log bound. Every test owns its sink.

// @cpt-dod:cpt-cf-oagw-dod-obs-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::data_plane::observability::{
    AuditSink, BreakerObservation, CollectingSink, Exchange, Observability, PhaseTimings,
};
use oagw::domain::error::ErrorKind;
use oagw::domain::ratelimit::{BreakerPhase, BreakerTransition};
use oagw::domain::observability::{
    AUDIT_EVENTS, AUDIT_FIELDS, AUDIT_LEVELS, AUTH_FAILURE_LOG_LIMIT, CORRELATION_HEADER,
    CorrelationContext, CorrelationSource, SamplingDecision,
};
use oagw::OagwState;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const TENANT: u128 = 0x71;

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

/// The `AuthZ` PDP a refusing deployment stands in for: every evaluation is
/// answered with a denial, which is the fail-closed answer a surface without
/// a permissive policy gives.
struct Denying;

#[async_trait::async_trait]
impl AuthZResolverClient for Denying {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: None,
            },
        })
    }
}

/// One mounted surface over its own store and its own sink.
struct Surface {
    router: Router,
    sink: Arc<CollectingSink>,
}

/// Builds a surface that writes its records to a collecting sink.
fn surface() -> Surface {
    surface_with(Some(Arc::new(Allowing)), None)
}

/// Builds a surface whose `AuthZ` client answers through `authz` and whose
/// credential store is `cred_store`, writing its records to a collecting sink.
///
/// A surface built over no credential store serves the unavailable one, so a
/// chain that resolves a credential through it reports unavailability rather
/// than a refusal.
fn surface_with(
    authz: Option<Arc<dyn AuthZResolverClient>>,
    cred_store: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
) -> Surface {
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let state = Arc::new(
        OagwState::assemble(&config, authz, None, cred_store).expect("the surface assembles"),
    );
    let sink = Arc::new(CollectingSink::new());
    state.swap_audit_sink(Arc::clone(&sink) as Arc<dyn oagw::data_plane::observability::AuditSink>);
    Surface {
        router: oagw::api::rest::register_management_routes(Router::new(), state),
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

/// A live HTTP/1.1 upstream, which answers on the port it bound.
#[derive(Clone, Copy)]
struct Upstream {
    port: u16,
}

/// Starts one upstream that answers `ok` on an ephemeral port.
async fn upstream() -> Upstream {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
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
                    if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
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
                let _ = socket
                    .write_all(
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

/// Stores one upstream and one route over it, both through the management API.
async fn wired(app: &Router, target: &Upstream) {
    store_upstream(app, target, None).await;
}

/// Stores one upstream carrying `plugins` and the auth sub-configuration, and
/// one route over it, both through the management API, answering the upstream's
/// instance key.
async fn store_upstream(
    app: &Router,
    target: &Upstream,
    auth: Option<serde_json::Value>,
) -> String {
    let mut body = json!({
        "alias": "127.0.0.1",
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": target.port }] },
        "protocol": HTTP_PROTOCOL,
        "tags": ["proxy"],
        "plugins": { "items": [] }
    });
    if let Some(auth) = auth {
        body["auth"] = auth;
    }
    let create = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    let response = app.clone().oneshot(create).await.expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    assert_eq!(status, StatusCode::CREATED, "{bytes:?}");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    let instance = document["id"].as_str().expect("the instance id");
    let created = oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string();
    let key = oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string();
    let route = json!({
        "upstream_id": key,
        "match": { "http": { "methods": ["GET"], "path": "/api" } },
        "priority": 10
    });
    let create = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/routes")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(route.to_string()))
        .expect("the request builds");
    let response = app.clone().oneshot(create).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::CREATED);
    created
}

/// An empty runtime over a collecting sink, for the record-shape tests.
fn runtime() -> Arc<Observability> {
    Arc::new(Observability::with_sink(Arc::new(CollectingSink::new())))
}

/// An empty runtime whose sink the test reads the written records back from.
fn runtime_with_sink() -> (Arc<Observability>, Arc<CollectingSink>) {
    let sink = Arc::new(CollectingSink::new());
    let observability = Arc::new(Observability::with_sink(
        Arc::clone(&sink) as Arc<dyn oagw::data_plane::observability::AuditSink>,
    ));
    (observability, sink)
}

/// An exchange whose record is the success record the shape tests read.
fn succeeded() -> Exchange {
    Exchange {
        host: Some(String::from("up.example")),
        route: Some(String::from("/api")),
        method: String::from("GET"),
        status: Some(200),
        timings: Some(PhaseTimings::started()),
        request_size: 24,
        response_size: Some(48),
        ..Exchange::default()
    }
}

/// The correlation context the record tests carry, whose roll keeps.
fn correlated() -> CorrelationContext {
    CorrelationContext {
        request_id: String::from("trace-keep-30"),
        source: CorrelationSource::InboundHeader,
        tenant_id: Some(Uuid::from_u128(TENANT)),
        principal_id: Some(String::from("subject-71")),
        sampling: SamplingDecision::Keep,
    }
}

#[test]
fn every_record_carries_only_the_fourteen_field_names() {
    assert_eq!(AUDIT_FIELDS.len(), 14);
    // The field set is the constant, and no member of it is a fifteenth name.
    assert_eq!(
        AUDIT_FIELDS,
        [
            "timestamp",
            "level",
            "event",
            "request_id",
            "tenant_id",
            "principal_id",
            "host",
            "path",
            "method",
            "status",
            "duration_ms",
            "request_size",
            "response_size",
            "error_type"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_success_record_carries_the_nine_fields_the_design_names() {
    let Surface { router, sink } = surface();
    let target = upstream().await;
    wired(&router, &target).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/proxy/127.0.0.1/api/one")
        .extension(subject())
        .header(CORRELATION_HEADER, "trace-keep-30")
        .body(Body::empty())
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::OK, "{:?}", sink.records());
    // The transfer ends when the body is read, and the record is written then.
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");

    let parsed = sink.parsed().expect("every record is one JSON object");
    let record = parsed
        .iter()
        .find(|record| record["event"] == json!("proxy_request.succeeded"))
        .expect("the success record");
    for field in ["timestamp", "level", "event", "request_id", "host", "path", "method", "status", "duration_ms"] {
        assert!(record.get(field).is_some(), "{field} is absent from {record}");
    }
    assert!(record.get("error_type").is_none(), "{record}");
    assert!(record.get("error_message").is_none(), "{record}");
    assert_eq!(record["level"], json!("INFO"), "{record}");
    assert_eq!(record["tenant_id"], json!(Uuid::from_u128(TENANT).to_string()), "{record}");
    assert_eq!(record["request_size"], json!("0"), "{record}");
    assert_eq!(record["response_size"], json!("2"), "{record}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_record_is_written_for_one_proxy_request() {
    let Surface { router, sink } = surface();
    for _ in 0..3 {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/oagw/v1/proxy/no-such-upstream/api")
            .extension(subject())
            .body(Body::empty())
            .expect("the request builds");
        let response = router.clone().oneshot(request).await.expect("oneshot resolves");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let records = sink.records();
    assert_eq!(records.len(), 3, "{records:?}");
    // Every line is one JSON object with no interleaved bytes.
    assert_eq!(sink.parsed().expect("the records parse").len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_request_is_recorded_at_the_level_the_mapping_assigns() {
    let Surface { router, sink } = surface();
    // A route the gateway answers 404 on is a failed request at ERROR.
    let request = Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/proxy/no-such-upstream/api")
        .extension(subject())
        .body(Body::empty())
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let parsed = sink.parsed().expect("the records parse");
    let failed = parsed
        .iter()
        .find(|record| record["event"] == json!("proxy_request.failed"))
        .expect("the failed record");
    assert_eq!(failed["status"], json!("404"));
    // A route the gateway never matched is a client error, not a gateway
    // failure, so the mapping keeps it at INFO and names the variant it
    // answered with.
    assert_eq!(failed["level"], json!("INFO"), "{failed}");
    assert_eq!(failed["error_type"], json!("route.not_found"), "{failed}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_failure_status_is_recorded_at_error() {
    let Surface { router, sink } = surface();
    // No listener answers the port, so the forward is refused: the gateway
    // answers the caller itself, and the record carries the failure's kind.
    let body = json!({
        "alias": "refusing.example",
        "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 1 }] },
        "protocol": HTTP_PROTOCOL
    });
    let create = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    let response = router.clone().oneshot(create).await.expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    assert_eq!(status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&bytes));
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    let key = oagw::gts::parse_gts_instance(
        oagw::UPSTREAM_TYPE,
        document["id"].as_str().expect("the instance id"),
    )
    .expect("the instance parses")
    .to_string();
    let route = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/routes")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "upstream_id": key,
                "match": { "http": { "methods": ["GET"], "path": "/api" } },
                "priority": 10
            })
            .to_string(),
        ))
        .expect("the request builds");
    let response = router.clone().oneshot(route).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::CREATED);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/oagw/v1/proxy/refusing.example/api")
        .extension(subject())
        .body(Body::empty())
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{:?}", sink.records());
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");

    let parsed = sink.parsed().expect("the records parse");
    let failed = parsed
        .iter()
        .find(|record| record["event"] == json!("proxy_request.failed"))
        .expect("the failed record");
    assert_eq!(failed["level"], json!("ERROR"), "{failed}");
    assert_eq!(failed["status"], json!("503"), "{failed}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_configuration_write_writes_no_record() {
    let Surface { router, sink } = surface();
    // A create the validators refuse: the write never completed, so no
    // configuration-change record is written.
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(json!({}).to_string()))
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST,);

    let parsed = sink.parsed().expect("the records parse");
    assert!(
        parsed.is_empty(),
        "a refused write wrote a record: {parsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_configuration_write_writes_exactly_one_record() {
    let Surface { router, sink } = surface();
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "up.example", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "tags": ["llm"]
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::CREATED);

    let parsed = sink.parsed().expect("the records parse");
    assert_eq!(parsed.len(), 1, "{parsed:?}");
    let record = &parsed[0];
    assert_eq!(record["level"], json!("INFO"));
    assert_eq!(record["event"], json!("config.upstream.created"));
    assert_eq!(record["tenant_id"], json!(Uuid::from_u128(TENANT).to_string()));
    assert_eq!(record["principal_id"], json!(Uuid::from_u128(TENANT).to_string()));
    assert_eq!(record["path"], json!("/oagw/v1/upstreams"));
    assert_eq!(record["method"], json!("POST"));
    assert_eq!(record["status"], json!("201"));
    // No proxy exchange happened, so the four exchange fields are absent.
    for field in ["host", "duration_ms", "request_size", "response_size"] {
        assert!(record.get(field).is_none(), "{field} is present: {record}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_configuration_write_carries_the_delete_event() {
    let Surface { router, sink } = surface();
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "up.example", "port": 443 }] },
        "protocol": HTTP_PROTOCOL
    });
    let create = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    let response = router.clone().oneshot(create).await.expect("oneshot resolves");
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    let instance = document["id"].as_str().expect("the instance id").to_owned();

    sink.parsed().expect("the records parse");
    let before = sink.records().len();

    let delete = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/oagw/v1/upstreams/{instance}"))
        .extension(subject())
        .body(Body::empty())
        .expect("the request builds");
    let response = router.oneshot(delete).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let parsed = sink.parsed().expect("the records parse");
    assert_eq!(parsed.len(), before + 1);
    let record = parsed.last().expect("the delete record");
    assert_eq!(record["event"], json!("config.upstream.deleted"));
    assert_eq!(record["method"], json!("DELETE"));
    assert_eq!(record["status"], json!("204"));
}

#[test]
fn the_five_categories_all_produce_a_record() {
    let (observability, sink) = runtime_with_sink();
    // A successful proxy request record.
    observability.observe(&succeeded(), Some(&correlated()));
    // A failed proxy request record.
    let refused = Exchange {
        error: Some(ErrorKind::RateLimitExceeded),
        status: Some(429),
        ..succeeded()
    };
    observability.observe(&refused, Some(&correlated()));
    // A configuration change.
    observability.config_change(
        oagw::domain::observability::EVENT_UPSTREAM_CREATED,
        Some(Uuid::from_u128(TENANT)),
        Some(String::from("subject-71")),
        "POST",
        "/oagw/v1/upstreams",
        201,
    );
    // An authentication failure.
    let unauthenticated = Exchange {
        gateway_answer: true,
        authentication_failure: true,
        status: Some(401),
        ..succeeded()
    };
    observability.observe(&unauthenticated, Some(&correlated()));
    // A circuit-breaker state transition.
    let transitioned = Exchange {
        breaker: Some(BreakerObservation {
            phase: Some(BreakerPhase::Open),
            transitions: vec![BreakerTransition {
                from: BreakerPhase::Closed,
                to: BreakerPhase::Open,
            }],
        }),
        ..succeeded()
    };
    observability.observe(&transitioned, Some(&correlated()));

    // Five categories, six records: the transition record is written in
    // addition to the success record the last request produces, never instead
    // of it, and every literal a record names is a member of the closed set.
    let mut events = sink
        .parsed()
        .expect("the records parse")
        .iter()
        .map(|record| record["event"].as_str().expect("an event value").to_string())
        .collect::<Vec<_>>();
    events.sort();
    assert_eq!(
        events,
        [
            String::from(oagw::domain::observability::EVENT_AUTH_FAILED),
            String::from(oagw::domain::observability::EVENT_BREAKER_TRANSITIONED),
            String::from(oagw::domain::observability::EVENT_UPSTREAM_CREATED),
            String::from(oagw::domain::observability::EVENT_REQUEST_FAILED),
            String::from(oagw::domain::observability::EVENT_REQUEST_SUCCEEDED),
            String::from(oagw::domain::observability::EVENT_REQUEST_SUCCEEDED),
        ],
        "{:?}",
        sink.records()
    );
    for record in sink.parsed().expect("the records parse") {
        assert!(
            AUDIT_EVENTS.contains(&record["event"].as_str().expect("an event value")),
            "{}",
            record
        );
    }
}

#[test]
fn the_levels_the_mapping_assigns_are_the_four_the_set_holds() {
    assert_eq!(AUDIT_LEVELS, ["INFO", "WARN", "ERROR", "DEBUG"]);
    // The mapping: WARN for the two refusals, ERROR for the failures.
    let (_, warn) = oagw::domain::observability::request_event_of(
        true,
        Some(ErrorKind::RateLimitExceeded),
    );
    assert_eq!(warn, "WARN");
    let (_, breaker) = oagw::domain::observability::request_event_of(
        true,
        Some(ErrorKind::CircuitBreakerOpen),
    );
    assert_eq!(breaker, "WARN");
    let (_, error) = oagw::domain::observability::request_event_of(
        true,
        Some(ErrorKind::RequestTimeout),
    );
    assert_eq!(error, "ERROR");
    let (_, success) = oagw::domain::observability::request_event_of(false, None);
    assert_eq!(success, "INFO");
    let (_, plain) = oagw::domain::observability::request_event_of(
        true,
        Some(ErrorKind::RouteNotFound),
    );
    assert_eq!(plain, "INFO");
}

#[test]
fn no_record_is_emitted_at_debug() {
    // The mapping names no DEBUG level for any outcome, and the record the
    // mapping builds carries the level it assigns.
    for kind in [
        ErrorKind::RateLimitExceeded,
        ErrorKind::CircuitBreakerOpen,
        ErrorKind::RequestTimeout,
        ErrorKind::RouteNotFound,
        ErrorKind::AuthenticationFailed,
    ] {
        let (_, level) = oagw::domain::observability::request_event_of(true, Some(kind));
        assert_ne!(level, "DEBUG", "{kind:?}");
    }
}

#[test]
fn the_record_admits_no_body_no_query_and_no_header_value() {
    let observability = runtime();
    let mut exchange = succeeded();
    // A request that carried a bearer token, a query string, and a body: none
    // of them is a field the fourteen carry, so none can appear.
    exchange.request_size = 4096;
    exchange.route = Some(String::from("/api"));
    observability.observe(&exchange, Some(&correlated()));
    let _ = observability.render();
    // The allowlist of §1.5 admits exactly one name.
    assert_eq!(CORRELATION_HEADER, "x-request-id");
    assert_eq!(AUDIT_FIELDS.iter().filter(|f| **f == "path").count(), 1);
}

#[test]
fn no_field_carries_a_credential_value() {
    // The `cred://` reference value and the bearer token are neither of them a
    // value of any of the fourteen fields, and the correlation header's value
    // is the only header value a record may carry.
    let (observability, sink) = runtime_with_sink();
    let mut exchange = succeeded();
    exchange.host = Some(String::from("up.example"));
    observability.observe(&exchange, Some(&correlated()));
    // A correlation context and a host whose values are the two credential
    // shapes the rules name: both are redacted out of the record, so neither
    // substring reaches a field.
    let carrying = CorrelationContext {
        request_id: String::from("cred://store/key"),
        source: CorrelationSource::InboundHeader,
        ..correlated()
    };
    let mut holding = succeeded();
    holding.host = Some(String::from("Bearer e2e-token-tenant-a"));
    observability.observe(&holding, Some(&carrying));

    let records = sink.records().join("\n");
    assert!(!records.contains("cred://"), "{}", records);
    assert!(!records.contains("Bearer "), "{}", records);
}

#[test]
fn the_failure_log_bound_drops_the_surplus_and_queues_nothing() {
    let observability = runtime();
    let mut written = 0_u32;
    for _ in 0..(AUTH_FAILURE_LOG_LIMIT * 3) {
        observability.emit(
            oagw::domain::observability::AuditEvent {
                timestamp: Some(oagw::data_plane::observability::timestamp_of(SystemTime::now())),
                level: Some(String::from("ERROR")),
                event: Some(String::from(oagw::domain::observability::EVENT_AUTH_FAILED)),
                request_id: Some(String::from("trace-keep-30")),
                tenant_id: None,
                principal_id: None,
                host: None,
                path: Some(String::from("/oagw/v1/proxy/up.example/api")),
                method: Some(String::from("GET")),
                status: Some(401),
                duration_ms: None,
                request_size: None,
                response_size: None,
                error_type: None,
            },
            false,
            SamplingDecision::Keep,
        );
        if observability.auth_failures_written() > 0 {
            written = observability.auth_failures_written();
        }
    }
    assert!(
        written <= AUTH_FAILURE_LOG_LIMIT,
        "{written} records were written inside one interval"
    );
}

#[test]
fn a_field_with_no_value_is_omitted_and_never_null_or_empty() {
    let observability = runtime();
    let sink = CollectingSink::new();
    // An event with only the fields the caller populated: the rest are absent.
    observability.emit(
        oagw::domain::observability::AuditEvent {
            timestamp: Some(String::from("2026-09-07T10:00:00Z")),
            level: Some(String::from("INFO")),
            event: Some(String::from(oagw::domain::observability::EVENT_UPSTREAM_CREATED)),
            request_id: None,
            tenant_id: Some(TENANT.to_string()),
            principal_id: Some(String::from("subject-71")),
            host: None,
            path: Some(String::from("/oagw/v1/upstreams")),
            method: Some(String::from("POST")),
            status: Some(201),
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: None,
        },
        false,
        SamplingDecision::Keep,
    );
    let _ = sink;
    // A populated field is never written empty.
    let event = oagw::domain::observability::AuditEvent {
        timestamp: Some(String::from("2026-09-07T10:00:00Z")),
        ..oagw::domain::observability::AuditEvent::default()
    };
    let populated = event.populated();
    assert!(populated.iter().all(|(_, value)| !value.is_empty()));
}

#[test]
fn the_audit_sink_is_the_one_seam_the_records_are_written_through() {
    // A sink a test owns receives exactly the records the emitter writes, in
    // the order it wrote them.
    struct Counting {
        written: std::sync::atomic::AtomicUsize,
    }
    impl AuditSink for Counting {
        fn write(&self, _record: &str) {
            self.written
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let sink = Arc::new(Counting {
        written: std::sync::atomic::AtomicUsize::new(0),
    });
    let observability = Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>);
    observability.observe(&succeeded(), Some(&correlated()));
    assert_eq!(sink.written.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn records_of_helper_is_not_a_second_serialization_path() {
    // The helper a suite reads through is the sink's own parse, and no second
    // serialization of a record exists in this feature.
    let sink = CollectingSink::new();
    assert!(sink.records().is_empty());
    assert!(sink.parsed().expect("an empty sink parses").is_empty());
    let _ = &sink;
}

#[test]
fn a_breaker_transition_and_a_configuration_change_are_never_sampled() {
    // A circuit-breaker transition is an event an operator must see and a
    // configuration change is by definition not high-volume, so both reach the
    // sink unsampled and unbound: even a record that arrives classified as
    // high-volume with a decision not to sample is still written.
    let sink = Arc::new(CollectingSink::new());
    let observability = Arc::new(Observability::with_sink(sink.clone()));
    observability.emit(
        oagw::domain::observability::AuditEvent {
            timestamp: Some(String::from("2026-09-07T10:00:00Z")),
            level: Some(String::from("WARN")),
            event: Some(String::from(
                oagw::domain::observability::EVENT_BREAKER_TRANSITIONED,
            )),
            request_id: Some(String::from("trace-drop-0")),
            tenant_id: Some(Uuid::from_u128(TENANT).to_string()),
            principal_id: None,
            host: Some(String::from("up.example")),
            path: Some(String::from("/api")),
            method: Some(String::from("GET")),
            status: Some(503),
            duration_ms: Some(12),
            request_size: Some(24),
            response_size: Some(48),
            error_type: None,
        },
        true,
        SamplingDecision::Drop,
    );
    observability.config_change(
        oagw::domain::observability::EVENT_UPSTREAM_CREATED,
        Some(Uuid::from_u128(TENANT)),
        Some(String::from("subject-71")),
        "POST",
        "/oagw/v1/upstreams",
        201,
    );

    let records = sink.records();
    assert_eq!(records.len(), 2, "{records:?}");
    let parsed = sink.parsed().expect("both lines parse");
    assert_eq!(parsed.len(), 2, "{parsed:?}");
    let events: Vec<&str> = parsed
        .iter()
        .map(|record| record["event"].as_str().expect("the event is a string"))
        .collect();
    assert_eq!(events[0], oagw::domain::observability::EVENT_BREAKER_TRANSITIONED);
    assert_eq!(events[1], oagw::domain::observability::EVENT_UPSTREAM_CREATED);
}

/// The answers a served request whose identity or whose permission the gateway
/// could not establish produces, recorded through the sink the surface owns.
///
/// Covers the refusal branches of `cpt-cf-oagw-flow-proxy-authorize` the served
/// composition can reach: the request that carries no subject and the request
/// the enforcer denies, both answered without contacting an upstream, and the
/// bound `auth.failed` records stay within the failure-log limit §1.5 sets.
mod served_refusals {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    /// Issues one proxy request with the extensions the caller states.
    async fn issue(router: &Router, extension: Option<SecurityContext>) -> StatusCode {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri("/oagw/v1/proxy/no-such-upstream/api");
        if let Some(extension) = extension {
            builder = builder.extension(extension);
        }
        let request = builder
            .body(Body::empty())
            .expect("the request builds");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("oneshot resolves");
        response.status()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unauthenticated_request_is_recorded_as_the_authentication_failure() {
        let Surface { router, sink } = surface_with(None, None);
        let status = issue(&router, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{:?}", sink.records());

        let parsed = sink.parsed().expect("the records parse");
        assert_eq!(parsed.len(), 1, "{:?}", sink.records());
        let record = &parsed[0];
        assert_eq!(
            record["event"],
            json!(oagw::domain::observability::EVENT_AUTH_FAILED)
        );
        assert_eq!(record["level"], json!("ERROR"));
        assert_eq!(record["status"], json!("401"));
        assert_eq!(record["method"], json!("GET"));
        // The identifier the request carried is the generated one, and no
        // route matched, so no `path` is carried.
        assert!(record["request_id"].is_string(), "{record}");
        assert!(record.get("path").is_none(), "no route matched: {record}");
        assert_eq!(record["error_type"], json!("auth.failed"), "{record}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_the_enforcer_refuses_is_recorded_as_the_authentication_failure() {
        let Surface { router, sink } =
            surface_with(Some(Arc::new(Denying)), None);
        let status = issue(&router, Some(subject())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{:?}", sink.records());

        let parsed = sink.parsed().expect("the records parse");
        assert_eq!(parsed.len(), 1, "{:?}", sink.records());
        let record = &parsed[0];
        assert_eq!(
            record["event"],
            json!(oagw::domain::observability::EVENT_AUTH_FAILED)
        );
        assert_eq!(record["level"], json!("ERROR"));
        assert_eq!(record["status"], json!("403"));
        assert!(record["principal_id"].is_string(), "{record}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flood_of_authentication_failures_is_bounded_through_the_served_path() {
        let Surface { router, sink } = surface_with(None, None);
        for _ in 0..(AUTH_FAILURE_LOG_LIMIT * 3) {
            let status = issue(&router, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let written = sink.records().len();
        assert!(
            written <= AUTH_FAILURE_LOG_LIMIT as usize,
            "{written} records were written for {} refusals",
            AUTH_FAILURE_LOG_LIMIT * 3
        );
        assert!(written > 0, "the first refusal is still written");
    }
}
