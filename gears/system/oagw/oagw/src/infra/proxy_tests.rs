//! Unit tests of the outbound proxy engine: endpoint selection, header
//! pipeline, body validation, error mapping and SSRF.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue};
use uuid::Uuid;

use super::*;
use crate::config::OagwConfig;
use crate::domain::metrics::MetricsRegistry;
use crate::domain::model::{
    Endpoint, HeaderRules, HeadersConfig, Protocol, Scheme, ServerConfig, Upstream,
};

fn endpoint(host: &str, scheme: Scheme) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port: 443,
    }
}

fn upstream(endpoints: Vec<Endpoint>) -> Upstream {
    upstream_named("payments-api", endpoints)
}

fn upstream_named(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    Upstream {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        alias: alias.to_owned(),
        tags: Vec::new(),
        protocol: Protocol::Http,
        server: ServerConfig { endpoints },
        auth: None,
        plugins: crate::domain::model::PluginConfig::default(),
        headers: HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: true,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

fn engine() -> ProxyEngine {
    ProxyEngine::new(&OagwConfig::default(), Arc::new(MetricsRegistry::new()))
}

// -- endpoint selection ----------------------------------------------------

#[test]
fn single_endpoint_wins_without_a_header() {
    let upstream = upstream(vec![endpoint("api.example.com", Scheme::Https)]);
    let selection = engine().select_endpoint(&upstream, None).unwrap();
    assert_eq!(selection.endpoint.host, "api.example.com");
    assert_eq!(selection.method, SelectionMethod::Default);
}

#[test]
fn explicit_header_matches_case_insensitively() {
    let upstream = upstream(vec![
        endpoint("payments-a.example.com", Scheme::Https),
        endpoint("payments-b.example.com", Scheme::Https),
    ]);
    let selection = engine()
        .select_endpoint(&upstream, Some("PAYMENTS-B.Example.COM"))
        .unwrap();
    assert_eq!(selection.endpoint.host, "payments-b.example.com");
    assert_eq!(selection.method, SelectionMethod::ExplicitHeader);
}

#[test]
fn explicit_header_accepts_a_port_and_fqdn_dot() {
    let upstream = upstream(vec![
        endpoint("payments-a.example.com", Scheme::Https),
        endpoint("payments-b.example.com", Scheme::Https),
    ]);
    let selection = engine()
        .select_endpoint(&upstream, Some("payments-b.example.com.:443"))
        .unwrap();
    assert_eq!(selection.endpoint.host, "payments-b.example.com");
}

#[test]
fn invalid_target_host_is_rejected() {
    let upstream = upstream(vec![
        endpoint("payments-a.example.com", Scheme::Https),
        endpoint("payments-b.example.com", Scheme::Https),
    ]);
    let error = engine()
        .select_endpoint(&upstream, Some("not a host"))
        .unwrap_err();
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

#[test]
fn unknown_target_host_lists_the_pool() {
    let upstream = upstream(vec![
        endpoint("payments-a.example.com", Scheme::Https),
        endpoint("payments-b.example.com", Scheme::Https),
    ]);
    let error = engine()
        .select_endpoint(&upstream, Some("payments-c.example.com"))
        .unwrap_err();
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.context().valid_hosts,
        vec![
            "payments-a.example.com".to_owned(),
            "payments-b.example.com".to_owned()
        ]
    );
}

#[test]
fn common_suffix_alias_demands_the_header() {
    // DESIGN section 3.1: the alias of a hostname pool *is* its registrable
    // common suffix, so `example.com` balancing two subdomains does not name
    // a single target.
    let upstream = upstream_named(
        "example.com",
        vec![
            endpoint("payments-eu.example.com", Scheme::Https),
            endpoint("payments-us.example.com", Scheme::Https),
        ],
    );
    let error = engine().select_endpoint(&upstream, None).unwrap_err();
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(error.context().valid_hosts.len(), 2);
}

#[test]
fn distinct_hosts_are_balanced_round_robin() {
    let upstream = upstream(vec![
        endpoint("eu.example.com", Scheme::Https),
        endpoint("us.example.com", Scheme::Https),
    ]);
    let engine = engine();
    let first = engine.select_endpoint(&upstream, None).unwrap();
    let second = engine.select_endpoint(&upstream, None).unwrap();
    let third = engine.select_endpoint(&upstream, None).unwrap();
    assert_eq!(first.method, SelectionMethod::RoundRobin);
    assert_eq!(first.endpoint.host, "eu.example.com");
    assert_eq!(second.endpoint.host, "us.example.com");
    assert_eq!(third.endpoint.host, "eu.example.com");
}

#[test]
fn empty_pool_is_link_unavailable() {
    let upstream = upstream(Vec::new());
    let error = engine().select_endpoint(&upstream, None).unwrap_err();
    assert_eq!(error.status(), http::StatusCode::SERVICE_UNAVAILABLE);
}

// -- header pipeline -------------------------------------------------------

#[test]
fn hop_by_hop_headers_are_stripped() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    headers.insert(
        HeaderName::from_static("keep-alive"),
        HeaderValue::from_static("timeout=5"),
    );
    headers.insert(
        http::header::TRANSFER_ENCODING,
        HeaderValue::from_static("chunked"),
    );
    headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(
        TARGET_HOST_HEADER,
        HeaderValue::from_static("eu.example.com"),
    );
    headers.insert("x-request-id", HeaderValue::from_static("r-1"));

    let stripped = strip_hop_by_hop(&headers);
    assert!(stripped.get(http::header::CONNECTION).is_none());
    assert!(stripped.get("keep-alive").is_none());
    assert!(stripped.get(http::header::TRANSFER_ENCODING).is_none());
    assert!(stripped.get(http::header::UPGRADE).is_none());
    assert!(stripped.get(TARGET_HOST_HEADER).is_none());
    assert_eq!(
        stripped.get("x-request-id"),
        Some(&HeaderValue::from_static("r-1"))
    );
}

#[test]
fn websocket_upgrade_is_detected() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    headers.insert(http::header::UPGRADE, HeaderValue::from_static("WebSocket"));
    assert!(is_websocket_upgrade(&headers));

    let mut missing = HeaderMap::new();
    missing.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
    assert!(!is_websocket_upgrade(&missing));

    let mut plain = HeaderMap::new();
    plain.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    plain.insert(http::header::UPGRADE, HeaderValue::from_static("h2c"));
    assert!(!is_websocket_upgrade(&plain));
}

#[test]
fn header_rules_run_set_then_add_then_remove() {
    let rules = HeaderRules {
        set: BTreeMap::from([("x-oagw-set".to_owned(), "final".to_owned())]),
        add: BTreeMap::from([("x-oagw-add".to_owned(), "one".to_owned())]),
        remove: vec!["x-oagw-drop".to_owned()],
        passthrough: Default::default(),
        passthrough_allowlist: Vec::new(),
    };
    let mut headers = HeaderMap::new();
    headers.insert("x-oagw-set", HeaderValue::from_static("original"));
    headers.insert("x-oagw-drop", HeaderValue::from_static("gone"));

    apply_header_rules(&mut headers, &rules).unwrap();
    assert_eq!(
        headers.get("x-oagw-set"),
        Some(&HeaderValue::from_static("final"))
    );
    assert_eq!(
        headers.get("x-oagw-add"),
        Some(&HeaderValue::from_static("one"))
    );
    assert!(headers.get("x-oagw-drop").is_none());
}

#[test]
fn invalid_header_rule_is_a_validation_error() {
    let rules = HeaderRules {
        set: BTreeMap::from([("bad name".to_owned(), "value".to_owned())]),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: Default::default(),
        passthrough_allowlist: Vec::new(),
    };
    let mut headers = HeaderMap::new();
    assert!(apply_header_rules(&mut headers, &rules).is_err());
}

#[test]
fn header_value_rejects_invalid_bytes() {
    assert!(header_value("bad\u{0}value").is_err());
    assert!(header_name("bad name").is_err());
    assert!(header_name("x-oagw-ok").is_ok());
}

// -- injected headers and query parameters ---------------------------------

/// A `GET /orders` request the engine turns into an outbound exchange.
fn outbound(request: ProxyRequest) -> hyper::Request<axum::body::Body> {
    let target = upstream(vec![endpoint("api.example.com", Scheme::Https)]);
    engine()
        .build_outbound_request(&target, None, request)
        .expect("outbound request builds")
}

/// A `GET /orders` request with a body-less default payload.
fn base_request() -> ProxyRequest {
    ProxyRequest {
        method: Method::GET,
        path: "/orders".to_owned(),
        ..ProxyRequest::default()
    }
}

/// Client headers plus an injected set, handed to the engine as-is otherwise.
fn outbound_with(
    headers: HeaderMap,
    injected: Vec<(&str, &str)>,
) -> hyper::Request<axum::body::Body> {
    let injected = injected
        .into_iter()
        .map(|(name, value)| {
            (
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            )
        })
        .collect();
    outbound(ProxyRequest {
        headers,
        injected_headers: injected,
        ..base_request()
    })
}

#[test]
fn injected_headers_override_the_client_value() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("spoofed"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let request = outbound_with(
        headers,
        vec![("x-api-key", "injected"), ("x-request-id", "r-1")],
    );

    assert_eq!(
        request.headers().get("x-api-key"),
        Some(&HeaderValue::from_static("injected"))
    );
    assert_eq!(
        request.headers().get_all("x-api-key").iter().count(),
        1,
        "an injected credential replaces the client value instead of appending to it"
    );
    assert_eq!(
        request.headers().get("x-request-id"),
        Some(&HeaderValue::from_static("r-1"))
    );
    assert_eq!(
        request.headers().get("content-type"),
        Some(&HeaderValue::from_static("application/json")),
        "the gateway-owned transport headers survive the injection"
    );
}

#[test]
fn injected_headers_survive_the_default_passthrough_mode() {
    let mut headers = HeaderMap::new();
    headers.insert("x-trace", HeaderValue::from_static("abc"));

    let request = outbound_with(headers, vec![("x-api-key", "injected")]);

    assert!(
        request.headers().get("x-trace").is_none(),
        "passthrough: none still strips ordinary client headers"
    );
    assert_eq!(
        request.headers().get("x-api-key"),
        Some(&HeaderValue::from_static("injected"))
    );
}

#[test]
fn injected_headers_survive_a_websocket_upgrade() {
    let mut headers = HeaderMap::new();
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));

    let request = ProxyRequest {
        headers,
        injected_headers: vec![(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("injected"),
        )],
        upgrade: true,
        ..base_request()
    };
    let request = outbound(request);

    assert_eq!(
        request.headers().get("x-api-key"),
        Some(&HeaderValue::from_static("injected"))
    );
    assert_eq!(
        request.headers().get(http::header::CONNECTION),
        Some(&HeaderValue::from_static("Upgrade")),
        "the upgrade pair is inserted after the injected set"
    );
    assert_eq!(
        request.headers().get(http::header::UPGRADE),
        Some(&HeaderValue::from_static("websocket"))
    );
}

#[test]
fn a_websocket_handshake_carries_the_client_negotiation_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("chat, superchat"),
    );
    headers.insert("x-client-only", HeaderValue::from_static("dropped"));

    let request = outbound(ProxyRequest {
        headers,
        upgrade: true,
        ..base_request()
    });

    assert_eq!(
        request.headers().get("sec-websocket-key"),
        Some(&HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ==")),
        "the server cannot answer 101 without the client key"
    );
    assert_eq!(
        request.headers().get("sec-websocket-version"),
        Some(&HeaderValue::from_static("13"))
    );
    assert_eq!(
        request.headers().get("sec-websocket-protocol"),
        Some(&HeaderValue::from_static("chat, superchat"))
    );
    assert!(
        request.headers().get("x-client-only").is_none(),
        "the passthrough posture still governs non-handshake headers"
    );
}

#[test]
fn a_plain_request_never_gains_handshake_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );

    let request = outbound(ProxyRequest {
        headers,
        ..base_request()
    });

    assert!(
        request.headers().get("sec-websocket-key").is_none(),
        "the negotiation headers are only carried on a real protocol switch"
    );
}

#[test]
fn the_outbound_content_length_is_taken_from_the_actual_buffer() {
    // The client declared 5 bytes, a transform plugin replaced the payload with
    // 20: a stale short header would truncate the body upstream.
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let request = outbound(ProxyRequest {
        headers,
        method: Method::POST,
        body: ProxyBody::Buffered(Bytes::from_static(&[b'x'; 20])),
        ..base_request()
    });

    assert_eq!(
        request.headers().get(http::header::CONTENT_LENGTH),
        Some(&HeaderValue::from_static("20")),
        "the buffer length is the outbound length"
    );
}

#[test]
fn a_bodyless_request_keeps_no_declared_content_length() {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("99"));

    let request = outbound(ProxyRequest {
        headers,
        ..base_request()
    });

    assert!(
        request
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .is_none(),
        "an empty payload must not inherit the client's declared length"
    );
}

#[test]
fn a_streamed_body_is_left_alone() {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));

    let request = outbound(ProxyRequest {
        headers,
        method: Method::POST,
        body: ProxyBody::Stream(axum::body::Body::from_stream(futures_util::stream::iter(
            vec![Ok::<_, std::convert::Infallible>(Bytes::from_static(
                b"payload",
            ))],
        ))),
        ..base_request()
    });

    // The rewrite only applies to a body resolved to concrete bytes: a
    // streaming body has no length to measure, so the pipeline keeps whatever
    // the client declared and `hyper` frames the exchange.
    assert_eq!(
        request.headers().get(http::header::CONTENT_LENGTH),
        Some(&HeaderValue::from_static("5"))
    );
}

#[test]
fn an_unchanged_query_is_forwarded_verbatim() {
    let request = outbound(ProxyRequest {
        query: "page=2&filter=name%20eq%20%27x%27".to_owned(),
        ..base_request()
    });
    assert_eq!(
        request.uri().query(),
        Some("page=2&filter=name%20eq%20%27x%27"),
        "no injection means no re-encoding of the caller's query"
    );
}

#[test]
fn injected_parameters_are_appended_in_injection_order() {
    let request = outbound(ProxyRequest {
        injected_query: vec![
            ("api_key".to_owned(), "first".to_owned()),
            ("tenant".to_owned(), "acme".to_owned()),
        ],
        ..base_request()
    });
    assert_eq!(request.uri().query(), Some("api_key=first&tenant=acme"));
}

#[test]
fn injected_parameters_override_the_client_parameter() {
    let request = outbound(ProxyRequest {
        query: "api_key=spoofed&keep=1".to_owned(),
        injected_query: vec![("api_key".to_owned(), "secret".to_owned())],
        ..base_request()
    });
    assert_eq!(
        request.uri().query(),
        Some("keep=1&api_key=secret"),
        "the client pair is dropped, the injected one is appended"
    );
}

#[test]
fn an_encoded_client_name_is_overridden_too() {
    let request = outbound(ProxyRequest {
        query: "api%5Fkey=spoofed&keep=1".to_owned(),
        injected_query: vec![("api_key".to_owned(), "secret".to_owned())],
        ..base_request()
    });
    assert_eq!(request.uri().query(), Some("keep=1&api_key=secret"));
}

#[test]
fn injected_parameters_are_url_encoded() {
    let request = outbound(ProxyRequest {
        query: "keep=1".to_owned(),
        injected_query: vec![("api key".to_owned(), "sec ret&x=1".to_owned())],
        ..base_request()
    });
    assert_eq!(
        request.uri().query(),
        Some("keep=1&api+key=sec+ret%26x%3D1")
    );
}

#[test]
fn a_leading_question_mark_never_reaches_the_uri() {
    let request = outbound(ProxyRequest {
        query: "?page=2".to_owned(),
        injected_query: vec![("api_key".to_owned(), "secret".to_owned())],
        ..base_request()
    });
    assert_eq!(request.uri().query(), Some("page=2&api_key=secret"));
}

#[test]
fn merge_query_keeps_the_client_bytes_verbatim() {
    assert_eq!(merge_query("a=1&b=%2Ftwo", &[]), "a=1&b=%2Ftwo");
    assert_eq!(merge_query("?a=1", &[]), "a=1");
    assert_eq!(merge_query("", &[]), "");
    assert_eq!(
        merge_query("a=1&b=%2Ftwo", &[("c".to_owned(), "3".to_owned())]),
        "a=1&b=%2Ftwo&c=3",
        "only the injected pairs are encoded"
    );
    assert_eq!(merge_query("", &[("k".to_owned(), "v".to_owned())]), "k=v");
}

// -- body validation -------------------------------------------------------

#[test]
fn matching_content_length_passes() {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("3"));
    assert_eq!(validate_body(&headers, Some(b"abc")).unwrap(), 3);
}

#[test]
fn missing_content_length_passes() {
    assert_eq!(validate_body(&HeaderMap::new(), None).unwrap(), 0);
}

#[test]
fn wrong_content_length_is_a_validation_error() {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));
    let error = validate_body(&headers, Some(b"abc")).unwrap_err();
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
}

#[test]
fn non_integer_content_length_is_a_validation_error() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from_static("three"),
    );
    assert_eq!(
        validate_body(&headers, Some(b"abc")).unwrap_err().status(),
        http::StatusCode::BAD_REQUEST
    );
}

#[test]
fn oversized_body_is_rejected() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from_str(&format!("{}", MAX_REQUEST_BODY_BYTES + 1)).unwrap(),
    );
    let error = validate_body(&headers, Some(&[0u8; 10])).unwrap_err();
    assert_eq!(error.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

#[test]
fn transfer_encoding_must_be_chunked() {
    let mut chunked = HeaderMap::new();
    chunked.insert(
        http::header::TRANSFER_ENCODING,
        HeaderValue::from_static("chunked"),
    );
    assert!(validate_body(&chunked, None).is_ok());

    let mut identity = HeaderMap::new();
    identity.insert(
        http::header::TRANSFER_ENCODING,
        HeaderValue::from_static("identity"),
    );
    assert_eq!(
        validate_body(&identity, None).unwrap_err().status(),
        http::StatusCode::BAD_REQUEST
    );

    let mut gzip = HeaderMap::new();
    gzip.insert(
        http::header::TRANSFER_ENCODING,
        HeaderValue::from_static("gzip"),
    );
    assert_eq!(
        validate_body(&gzip, None).unwrap_err().status(),
        http::StatusCode::BAD_REQUEST
    );
}

// -- error mapping ---------------------------------------------------------

#[test]
fn tls_and_handshake_failures_are_protocol_errors() {
    let upstream = upstream(vec![endpoint("api.example.com", Scheme::Https)]);
    let target = endpoint("api.example.com", Scheme::Https);
    let error = pingora_core::Error::new(pingora_core::ErrorType::TLSHandshakeFailure);
    let mapped = map_connect_error(&error, &upstream, &target);
    assert_eq!(mapped.status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        mapped.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
    );
    assert_eq!(mapped.context().host.as_deref(), Some("api.example.com"));
}

#[test]
fn connect_timeout_is_a_gateway_timeout() {
    let upstream = upstream(vec![endpoint("api.example.com", Scheme::Https)]);
    let target = endpoint("api.example.com", Scheme::Https);
    let error = pingora_core::Error::new(pingora_core::ErrorType::ConnectTimedout);
    let mapped = map_connect_error(&error, &upstream, &target);
    assert_eq!(mapped.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        mapped.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1"
    );
}

#[test]
fn connect_refused_is_link_unavailable() {
    let upstream = upstream(vec![endpoint("api.example.com", Scheme::Http)]);
    let target = endpoint("api.example.com", Scheme::Http);
    let error = pingora_core::Error::new(pingora_core::ErrorType::ConnectRefused);
    let mapped = map_connect_error(&error, &upstream, &target);
    assert_eq!(mapped.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        mapped.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[test]
fn exchange_timeout_is_a_request_timeout() {
    let target = endpoint("api.example.com", Scheme::Https);
    let error = classify_exchange(true, false, false, &target);
    assert_eq!(error.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        error.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
}

#[test]
fn aborted_or_truncated_stream_is_a_stream_abort() {
    let target = endpoint("api.example.com", Scheme::Https);
    let aborted = classify_exchange(false, true, false, &target);
    assert_eq!(aborted.status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        aborted.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
    );
    let truncated = classify_exchange(false, false, true, &target);
    assert_eq!(
        truncated.problem_body().r#type,
        "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
    );
}

#[test]
fn exchange_parse_error_is_a_protocol_error() {
    let upstream = upstream(vec![endpoint("api.example.com", Scheme::Https)]);
    let target = endpoint("api.example.com", Scheme::Https);
    let error = enrich(
        classify_exchange(false, false, false, &target),
        &upstream,
        &target,
    );
    assert_eq!(error.status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.context().host.as_deref(), Some("api.example.com"));
}

// -- tls decision ----------------------------------------------------------

#[test]
fn tls_schemes_enable_tls() {
    assert!(is_tls(Scheme::Https));
    assert!(is_tls(Scheme::Wss));
    assert!(is_tls(Scheme::Grpc));
    assert!(!is_tls(Scheme::Http));
    assert!(!is_tls(Scheme::Ws));
}

#[test]
fn peers_carry_the_proxy_budget() {
    let target = endpoint("api.example.com", Scheme::Https);
    let peer = build_peer(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        &target,
        Duration::from_secs(3),
    );
    assert_eq!(
        peer.options.connection_timeout,
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        peer.options.total_connection_timeout,
        Some(Duration::from_secs(3))
    );
    assert!(peer.is_tls());
    assert_eq!(peer.sni, "api.example.com");
}

// -- host normalisation ----------------------------------------------------

#[test]
fn requested_host_normalisation_strips_the_port() {
    assert_eq!(
        normalize_requested_host("Api.Example.com:8443").unwrap(),
        "api.example.com"
    );
    assert_eq!(
        normalize_requested_host("api.example.com.").unwrap(),
        "api.example.com"
    );
    assert_eq!(normalize_requested_host("10.0.0.1").unwrap(), "10.0.0.1");
    assert_eq!(
        normalize_requested_host("[2001:db8::1]:443").unwrap(),
        "2001:db8::1"
    );
    assert!(normalize_requested_host("not a host").is_err());
    assert!(normalize_requested_host("bad/host").is_err());
}

#[test]
fn host_parsing_tolerates_an_unbracketed_colon() {
    assert_eq!(strip_port("example.com:443"), "example.com");
    assert_eq!(strip_port("example.com"), "example.com");
    assert_eq!(strip_port("[2001:db8::1]"), "2001:db8::1");
    assert_eq!(strip_port("[2001:db8::1]:443"), "2001:db8::1");
}

// -- SSRF ------------------------------------------------------------------

#[test]
fn ssrf_disabled_allows_private_targets() {
    let policy = crate::config::SsrfPolicy::default();
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok());
}

#[test]
fn ssrf_blocks_loopback_when_private_networks_are_forbidden() {
    let policy = crate::config::SsrfPolicy {
        enabled: true,
        allow_private_networks: false,
        allowed_ip_ranges: Vec::new(),
    };
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err());
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))).is_err());
    assert!(check_ssrf(&policy, IpAddr::V6("fe80::1".parse().unwrap())).is_err());
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_ok());
}

#[test]
fn ssrf_enforces_the_cidr_allowlist() {
    let policy = crate::config::SsrfPolicy {
        enabled: true,
        allow_private_networks: true,
        allowed_ip_ranges: vec!["10.0.0.0/8".to_owned(), "2001:db8::/32".to_owned()],
    };
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))).is_ok());
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(11, 1, 2, 3))).is_err());
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))).is_err());
    assert!(check_ssrf(&policy, IpAddr::V6("2001:db8:1::1".parse().unwrap())).is_ok());
    assert!(check_ssrf(&policy, IpAddr::V6("fe80::1".parse().unwrap())).is_err());
}

#[test]
fn ssrf_classifies_an_ipv4_mapped_ipv6_address_as_ipv4() {
    let policy = crate::config::SsrfPolicy {
        enabled: true,
        allow_private_networks: false,
        allowed_ip_ranges: Vec::new(),
    };
    // (address, expected allowed)
    let cases = [
        // The mapped forms of the ranges the gate exists to block.
        ("::ffff:127.0.0.1", false),
        ("::ffff:10.0.0.5", false),
        ("::ffff:192.168.1.4", false),
        ("::ffff:169.254.1.1", false),
        ("::ffff:0.0.0.0", false),
        // ... and the IPv6 forms, which were never in question.
        ("::1", false),
        ("::", false),
        ("fd00::1", false),
        ("fe80::1", false),
        ("169.254.1.1", false),
        ("0.0.0.0", false),
        // Global unicast in both spellings is the one thing allowed through.
        ("::ffff:8.8.8.8", true),
        ("8.8.8.8", true),
        ("2001:db8::1", true),
        ("2606:4700:4700::1111", true),
    ];
    for (address, allowed) in cases {
        let address: IpAddr = address.parse().expect("test address parses");
        assert_eq!(
            check_ssrf(&policy, address).is_ok(),
            allowed,
            "{address} classified wrong"
        );
    }
}

#[test]
fn a_mapped_private_address_matches_an_ipv4_cidr_allowlist() {
    let policy = crate::config::SsrfPolicy {
        enabled: true,
        allow_private_networks: true,
        allowed_ip_ranges: vec!["10.0.0.0/8".to_owned()],
    };
    assert!(check_ssrf(&policy, IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))).is_ok());
    assert!(
        check_ssrf(&policy, IpAddr::V6("::ffff:10.1.2.3".parse().unwrap())).is_ok(),
        "the mapped spelling must resolve to the same network"
    );
    assert!(check_ssrf(&policy, IpAddr::V6("::ffff:11.1.2.3".parse().unwrap())).is_err());
}

#[test]
fn cidr_matching_ignores_malformed_ranges() {
    assert!(!cidr_contains("nonsense", IpAddr::V4(Ipv4Addr::LOCALHOST)));

    assert!(!cidr_contains(
        "10.0.0.0/33",
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))
    ));
    assert!(!cidr_contains(
        "10.0.0.0",
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))
    ));
    assert!(!cidr_contains(
        "10.0.0.0/8",
        IpAddr::V6("2001:db8::1".parse().unwrap())
    ));
}

// -- metrics ---------------------------------------------------------------

#[test]
fn selections_are_reported_to_the_registry() {
    let metrics = Arc::new(MetricsRegistry::new());
    let engine = ProxyEngine::new(&OagwConfig::default(), Arc::clone(&metrics));
    let upstream = upstream(vec![
        endpoint("eu.example.com", Scheme::Https),
        endpoint("us.example.com", Scheme::Https),
    ]);
    let _ = engine.select_endpoint(&upstream, None).unwrap();
    let rendered = metrics.render();
    assert!(rendered.contains(
        "oagw_routing_target_host_used{upstream_id=\"00000000-0000-0000-0000-000000000000\",\
         endpoint_host=\"eu.example.com\"}"
    ));
    assert!(rendered.contains("selection_method=\"round_robin\""));
    assert!(rendered.contains(
        "oagw_upstream_available{host=\"payments-api\",\
         endpoint=\"https://eu.example.com:443\"} 1"
    ));
}

#[test]
fn debug_never_dumps_the_connector() {
    let engine = engine();
    let rendered = format!("{engine:?}");
    assert!(rendered.contains("ProxyEngine"));
    assert!(rendered.contains("timeout"));
}
