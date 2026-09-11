//! Tests of the WebSocket session relay
//! (`cpt-cf-oagw-flow-request-proxy-websocket-session`).

use futures_util::future::pending;
use uuid::Uuid;

use super::upgrade::{InboundUpgrade, UpstreamClient, relay};
use crate::domain::dto::{Endpoint, EndpointScheme, HeadersConfig};
use crate::domain::headers::build_request_headers;
use crate::domain::proxy::{ProxyContext, StreamKind};
use crate::test_support::stub_upstream;

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Http, host: host.to_owned(), port }
}

fn context() -> ProxyContext {
    ProxyContext {
        method: "GET".to_owned(),
        alias: "api.vendor.com".to_owned(),
        path_suffix: Some("/v1/ws".to_owned()),
        query: None,
        headers: vec![
            ("host".to_owned(), "api.vendor.com".to_owned()),
            ("connection".to_owned(), "Upgrade".to_owned()),
            ("upgrade".to_owned(), "websocket".to_owned()),
            ("sec-websocket-key".to_owned(), "dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
        ],
        body: bytes::Bytes::new(),
        tenant_id: Uuid::new_v4(),
        principal_id: Uuid::new_v4(),
        peer_addr: Some("10.0.0.1:5000".to_owned()),
        trace_id: Some("trace-1".to_owned()),
    }
}

fn never_resolves() -> InboundUpgrade {
    InboundUpgrade { client: Box::pin(pending()) }
}

#[tokio::test]
async fn an_upstream_that_refuses_the_upgrade_fails_the_session() {
    // The stub answers the handshake with an ordinary 200, which is a refusal.
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let context = context();
    let outbound =
        build_request_headers(&context.headers, None, &endpoint(&host, port), true).expect("headers");
    let client = UpstreamClient::new();

    let outcome = relay(
        &client,
        &context,
        &endpoint(&host, port),
        outbound,
        None::<&HeadersConfig>,
        never_resolves(),
        context.trace_id.clone(),
    )
    .await;

    let error = outcome.expect_err("a 200 is not an upgrade");
    assert!(
        matches!(error, crate::domain::DomainError::DownstreamError { host: ref refused_host, .. } if refused_host.as_deref() == Some(host.as_str())),
        "the refusal names the upstream it refused: {error}"
    );
    // The refusal is a lifecycle event of its own, before any session opens.
    assert_eq!(stub.received()[0].header("upgrade"), Some("websocket"));
    assert!(stub.received()[0].header("connection").is_some());
    assert!(stub.received()[0].target().starts_with("/v1/ws"));
}

#[tokio::test]
async fn an_accepted_upgrade_returns_the_101_head_and_relays() {
    // The stub completes the handshake; the client leg of this test never
    // resolves, so the relay task records the abort once it polls the client.
    let stub = stub_upstream(vec![
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\n\r\n"
            .to_owned(),
    ])
    .await;
    let (host, port) = stub.endpoint();
    let context = context();
    let outbound =
        build_request_headers(&context.headers, None, &endpoint(&host, port), true).expect("headers");
    let client = UpstreamClient::new();

    let response = relay(
        &client,
        &context,
        &endpoint(&host, port),
        outbound,
        None::<&HeadersConfig>,
        never_resolves(),
        context.trace_id.clone(),
    )
    .await
    .expect("the handshake is accepted");

    assert_eq!(response.status, http::StatusCode::SWITCHING_PROTOCOLS.as_u16());
    assert_eq!(response.stream, StreamKind::WebSocket);
    assert_eq!(response.source, crate::domain::proxy::ErrorSource::Upstream);
    assert!(response.headers.iter().any(|(name, value)| name == "upgrade" && value == "websocket"));
    // The upstream handshake reached the upstream unstripped.
    assert_eq!(stub.received()[0].header("sec-websocket-key"), Some("dGhlIHNhbXBsZSBub25jZQ=="));
}
