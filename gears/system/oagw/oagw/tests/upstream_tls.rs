//! The TLS leg of the upstream connector.
//!
//! `cpt-cf-oagw-constraint-https-only` makes TLS the default posture, so the
//! connector's rustls path has to be exercised — including its failure mode.
//! Certificate verification is on and there is no per-upstream trust anchor
//! configuration, so a self-signed upstream is refused: the point of these
//! tests is that the handshake is genuinely attempted and the refusal
//! surfaces as a clean gateway error rather than a panic or a hang.

mod common;

use std::sync::Arc;

use common::{Fixture, assert_gateway_problem, get};
use http::StatusCode;
use oagw::domain::gts_helpers::errors;
use oagw::domain::model::{Endpoint, Scheme, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// A TLS listener presenting a freshly minted self-signed certificate.
async fn self_signed_https_server() -> std::net::SocketAddr {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("generate a self-signed certificate");
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(certified.signing_key.serialize_der())
        .expect("serialize the signing key");

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("build a server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().expect("listener address");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\
                              content-type: application/json\r\n\r\n{}",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    addr
}

/// Point an upstream at `addr` over `scheme`, route `/v1` to it, and return
/// the alias the gear derived. `localhost` is a hostname, so the alias is
/// auto-derived as `localhost:{port}` — the operator does not get to choose.
async fn https_upstream(fx: &Fixture, addr: std::net::SocketAddr, scheme: Scheme) -> String {
    let upstream = fx
        .gateway
        .control_plane
        .create_upstream(
            &fx.gateway.security_context,
            oagw::domain::services::management::UpstreamSpec {
                alias: None,
                enabled: None,
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme,
                        // `localhost` is what the certificate names; the
                        // SSRF policy is off in the harness, so it resolves
                        // freely.
                        host: "localhost".to_owned(),
                        port: Some(addr.port()),
                    }],
                },
                protocol: oagw::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        )
        .await
        .expect("create upstream");
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;
    upstream.alias
}

#[tokio::test]
async fn an_untrusted_upstream_certificate_is_refused_cleanly() {
    let fx = Fixture::start().await;
    let addr = self_signed_https_server().await;
    let alias = https_upstream(&fx, addr, Scheme::Https).await;

    let res = get(&fx.proxy_url(&alias, "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::SERVICE_UNAVAILABLE,
        errors::LINK_UNAVAILABLE,
    );
    assert!(
        res.json()["detail"]
            .as_str()
            .is_some_and(|d| d.contains("could not connect")),
        "the refusal is reported as a connection failure, not an opaque 500"
    );
}

#[tokio::test]
async fn a_tls_scheme_endpoint_never_falls_back_to_plaintext() {
    let fx = Fixture::start().await;
    // A plain HTTP listener behind an `https` endpoint: the handshake must
    // fail rather than silently downgrade.
    let upstream = fx
        .upstream("downgrade", |spec| {
            spec.server = ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: fx.upstream.host(),
                    port: Some(fx.upstream.port()),
                }],
            };
        })
        .await;
    fx.route(&upstream, &["GET"], "/v1", |_| {}).await;

    let res = get(&fx.proxy_url("downgrade", "v1/echo")).await;
    assert!(
        res.status.is_server_error(),
        "an https endpoint pointed at a plaintext port must not succeed; got {}",
        res.status
    );
    assert_eq!(res.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn the_wss_scheme_uses_the_same_tls_path() {
    let fx = Fixture::start().await;
    let addr = self_signed_https_server().await;
    let alias = https_upstream(&fx, addr, Scheme::Wss).await;

    // Same outcome as `https`: `wss` is the TLS member of the WebSocket
    // family and shares the connector.
    let res = get(&fx.proxy_url(&alias, "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::SERVICE_UNAVAILABLE,
        errors::LINK_UNAVAILABLE,
    );
}
