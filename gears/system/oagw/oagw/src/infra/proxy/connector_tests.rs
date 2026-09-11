//! Transport policy: scheme gating, SSRF guardrails and outbound framing.

use super::*;
use http::HeaderMap;
use crate::config::SsrfPolicy;
use crate::domain::model::Scheme;

fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn config(allow_http: bool, ssrf: SsrfPolicy) -> OagwConfig {
    OagwConfig {
        allow_http_upstream: allow_http,
        ssrf_policy: ssrf,
        ..OagwConfig::default()
    }
}

#[test]
fn the_authority_omits_the_scheme_default_port() {
    assert_eq!(
        upstream_authority(&endpoint(Scheme::Https, "api.example.com", 443)),
        "api.example.com"
    );
    assert_eq!(
        upstream_authority(&endpoint(Scheme::Http, "api.example.com", 80)),
        "api.example.com"
    );
    assert_eq!(
        upstream_authority(&endpoint(Scheme::Http, "api.example.com", 8080)),
        "api.example.com:8080"
    );
    assert_eq!(
        upstream_authority(&endpoint(Scheme::Ws, "chat.example.com", 80)),
        "chat.example.com"
    );
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_unless_the_flag_lifts_the_default() {
    let connector = UpstreamConnector::new(&config(false, SsrfPolicy { enabled: false, ..SsrfPolicy::default() }));
    let err = connector
        .peer_for(&endpoint(Scheme::Http, "127.0.0.1", 9099))
        .await
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("allow_http_upstream"), "{}", err.detail);
}

#[tokio::test]
async fn a_plaintext_upstream_is_dialled_once_the_flag_is_set() {
    let connector = UpstreamConnector::new(&config(true, SsrfPolicy { enabled: false, ..SsrfPolicy::default() }));
    let peer = connector
        .peer_for(&endpoint(Scheme::Http, "127.0.0.1", 9099))
        .await
        .unwrap();
    assert!(!peer.is_tls());
    assert_eq!(peer.sni, "127.0.0.1");
}

#[tokio::test]
async fn a_tls_upstream_needs_no_flag() {
    let connector = UpstreamConnector::new(&config(false, SsrfPolicy { enabled: false, ..SsrfPolicy::default() }));
    let peer = connector
        .peer_for(&endpoint(Scheme::Https, "127.0.0.1", 8443))
        .await
        .unwrap();
    assert!(peer.is_tls());
}

#[tokio::test]
async fn the_ssrf_policy_blocks_loopback_by_default() {
    let connector = UpstreamConnector::new(&config(true, SsrfPolicy::default()));
    let err = connector
        .peer_for(&endpoint(Scheme::Http, "127.0.0.1", 9099))
        .await
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("loopback"), "{}", err.detail);
}

#[tokio::test]
async fn the_ssrf_policy_blocks_private_and_link_local_ranges() {
    let connector = UpstreamConnector::new(&config(true, SsrfPolicy::default()));
    for (host, reason) in [("10.0.1.1", "private"), ("169.254.169.254", "link-local")] {
        let err = connector
            .peer_for(&endpoint(Scheme::Http, host, 80))
            .await
            .unwrap_err();
        assert!(err.detail.contains(reason), "{host}: {}", err.detail);
    }
}

#[tokio::test]
async fn loopback_can_be_allowed_explicitly() {
    let policy = SsrfPolicy {
        enabled: true,
        allow_loopback: true,
        ..SsrfPolicy::default()
    };
    let connector = UpstreamConnector::new(&config(true, policy));
    assert!(
        connector
            .peer_for(&endpoint(Scheme::Http, "127.0.0.1", 9099))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn blocked_ports_are_refused() {
    let policy = SsrfPolicy {
        enabled: true,
        allow_loopback: true,
        blocked_ports: vec![25],
        ..SsrfPolicy::default()
    };
    let connector = UpstreamConnector::new(&config(true, policy));
    let err = connector
        .peer_for(&endpoint(Scheme::Http, "127.0.0.1", 25))
        .await
        .unwrap_err();
    assert!(err.detail.contains("port 25"), "{}", err.detail);
}

#[tokio::test]
async fn an_unresolvable_host_is_a_link_failure_not_a_panic() {
    let connector = UpstreamConnector::new(&config(true, SsrfPolicy { enabled: false, ..SsrfPolicy::default() }));
    let err = connector
        .peer_for(&endpoint(Scheme::Http, "no-such-host.invalid", 80))
        .await
        .unwrap_err();
    assert_eq!(err.status(), 503);
}

#[tokio::test]
async fn ipv6_literals_are_accepted_in_bracketed_form() {
    let policy = SsrfPolicy {
        enabled: true,
        allow_loopback: true,
        ..SsrfPolicy::default()
    };
    let connector = UpstreamConnector::new(&config(true, policy));
    assert!(
        connector
            .peer_for(&endpoint(Scheme::Http, "[::1]", 9099))
            .await
            .is_ok()
    );
}

#[test]
fn the_outbound_head_replaces_host_and_frames_the_body() {
    let request = ProxyRequest {
        method: http::Method::POST,
        path: "/v1/chat".to_owned(),
        query: vec![("model".to_owned(), "gpt-4".to_owned())],
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{\"a\":1}"),
    };
    let header = build_request_header(&endpoint(Scheme::Https, "api.example.com", 443), &request)
        .unwrap();
    assert_eq!(header.uri.path_and_query().unwrap(), "/v1/chat?model=gpt-4");
    assert_eq!(header.headers.get(http::header::HOST).unwrap(), "api.example.com");
    assert_eq!(header.headers.get(http::header::CONTENT_LENGTH).unwrap(), "7");
}

#[test]
fn a_bodyless_write_method_still_declares_a_zero_length() {
    let request = ProxyRequest {
        method: http::Method::POST,
        path: "/v1/ping".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
    };
    let header = build_request_header(&endpoint(Scheme::Https, "api.example.com", 443), &request)
        .unwrap();
    assert_eq!(header.headers.get(http::header::CONTENT_LENGTH).unwrap(), "0");
}

#[test]
fn a_bodyless_get_declares_no_length_at_all() {
    let request = ProxyRequest {
        method: http::Method::GET,
        path: "/v1/models".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
    };
    let header = build_request_header(&endpoint(Scheme::Https, "api.example.com", 443), &request)
        .unwrap();
    assert!(header.headers.get(http::header::CONTENT_LENGTH).is_none());
}
