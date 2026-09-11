//! Tests of the data-plane DTOs
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`).

use crate::domain::proxy::*;
use bytes::Bytes;

fn context() -> ProxyContext {
    ProxyContext {
        method: "GET".to_owned(),
        alias: "api.vendor.com".to_owned(),
        path_suffix: Some("/v1/orders".to_owned()),
        query: Some("limit=1".to_owned()),
        headers: vec![("host".to_owned(), "api.vendor.com".to_owned())],
        body: Bytes::new(),
        tenant_id: uuid::Uuid::nil(),
        principal_id: uuid::Uuid::nil(),
        peer_addr: None,
        trace_id: None,
    }
}

#[test]
fn a_suffix_is_composed_with_its_leading_slash() {
    assert_eq!(context().request_path(), "/v1/orders");
}

#[test]
fn a_request_without_a_suffix_addresses_the_root() {
    let mut context = context();
    context.path_suffix = None;
    assert_eq!(context.request_path(), "/");
}

#[test]
fn an_empty_suffix_is_the_root_too() {
    let mut context = context();
    context.path_suffix = Some(String::new());
    assert_eq!(context.request_path(), "/");
}

#[test]
fn the_error_source_names_its_header_value() {
    assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
    assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
}

#[test]
fn the_stream_kinds_name_the_transport() {
    assert!(!StreamKind::None.is_streaming());
    assert!(StreamKind::Sse.is_streaming());
    assert!(StreamKind::WebSocket.is_streaming());
    assert!(StreamKind::WebTransport.is_streaming());
    assert_eq!(StreamKind::Sse.as_str(), "sse");
    assert_eq!(StreamKind::WebSocket.as_str(), "ws");
    assert_eq!(StreamKind::WebTransport.as_str(), "wt");
}

#[test]
fn the_lifecycle_records_every_event_exactly_once() {
    let lifecycle = StreamLifecycle::shared();
    lifecycle.record(StreamEvent::Refused);
    lifecycle.record(StreamEvent::Open);
    lifecycle.record(StreamEvent::Close);
    assert_eq!(
        lifecycle.events(),
        vec![StreamEvent::Refused, StreamEvent::Open, StreamEvent::Close]
    );
    assert!(lifecycle.contains(StreamEvent::Open));
    assert!(!lifecycle.contains(StreamEvent::Aborted));
}

#[test]
fn the_observation_carries_the_pipeline_boundary_values() {
    let observation = ProxyObservation {
        status: 200,
        duration_ms: 12,
        request_size: 34,
        response_size: 56,
        error_type: None,
        rate_limit: None,
        cors: None,
        host: Some("api.vendor.com".to_owned()),
        route: Some("/v1".to_owned()),
        routing: None,
        phases: PhaseObservation::default(),
    };
    assert_eq!(observation.status, 200);
    assert_eq!(observation.duration_ms, 12);
    assert_eq!(observation.request_size, 34);
    assert_eq!(observation.response_size, 56);
}
