//! Tests for the dialer's gates and the shapes it hands to hyper.
//!
//! Only the two gates — the scheme gate and the SSRF screen — are asserted
//! here: a dial that reaches the network is not a unit test, it is an
//! integration test, and it is covered by the data-plane tests that stub the
//! resolver instead of the socket.
use super::{Connection, UpstreamDialer};
use crate::domain::model::EndpointScheme;
use crate::infra::proxy::ssrf::SsrfPolicy;

fn dialer(policy: SsrfPolicy, allow_http: bool) -> UpstreamDialer {
    UpstreamDialer::new(
        std::sync::Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        policy,
        allow_http,
    )
}

#[tokio::test]
async fn a_plaintext_scheme_is_refused_when_http_is_not_allowed() {
    let host = "93.184.216.34";
    let failure = dialer(SsrfPolicy::disabled(), false)
        .dial(EndpointScheme::Http, host, 80)
        .await
        .expect_err("plaintext must be refused");
    assert_eq!(failure.status, 400);
    assert!(failure.detail.contains("plaintext scheme"));
    assert!(failure.detail.contains(host));
}

#[tokio::test]
async fn a_secure_scheme_is_not_subject_to_the_plaintext_gate() {
    // The dial still fails — there is no DNS in the test sandbox — but the
    // refusal must be a *resolution* failure, not a scheme refusal.
    let failure = dialer(SsrfPolicy::disabled(), false)
        .dial(EndpointScheme::Https, "not-a-real-host.invalid", 443)
        .await
        .expect_err("resolution must fail");
    assert_eq!(failure.status, 503);
    assert!(failure.detail.contains("cannot resolve upstream host"));
}

#[tokio::test]
async fn a_screened_address_is_refused_before_any_socket_is_opened() {
    let failure = dialer(SsrfPolicy { enabled: true }, true)
        .dial(EndpointScheme::Http, "127.0.0.1", 8080)
        .await
        .expect_err("loopback must be refused");
    assert_eq!(failure.status, 403);
}

#[tokio::test]
async fn a_disallowed_plaintext_scheme_is_caught_before_the_ssrf_screen() {
    let failure = dialer(SsrfPolicy { enabled: true }, false)
        .dial(EndpointScheme::Http, "127.0.0.1", 8080)
        .await
        .expect_err("plaintext must be refused first");
    assert_eq!(failure.status, 400);
}

#[tokio::test]
async fn an_ip_literal_is_screened_directly_without_dns() {
    let failure = dialer(SsrfPolicy { enabled: true }, true)
        .dial(EndpointScheme::Http, "10.0.0.7", 80)
        .await
        .expect_err("private space must be refused");
    assert_eq!(failure.status, 403);
}

#[tokio::test]
async fn a_loopback_tcp_stream_can_be_wrapped_for_hyper() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("stream");
    let connection = Connection::Plain(hyper_util::rt::TokioIo::new(stream));
    assert!(!connection.is_tls());
}
