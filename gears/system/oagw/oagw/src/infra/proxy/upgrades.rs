//! WebSocket (and generic `Upgrade`) tunnelling.
//!
//! axum's `ws` support sits behind its test-only feature set, so the handshake
//! is relayed **manually**:
//!
//! 1. the client's handshake request is forwarded verbatim — the
//!    `Connection`/`Upgrade` pair is preserved instead of stripped, which is why
//!    the pipeline calls
//!    [`strip_hop_by_hop`](crate::infra::proxy::headers::strip_hop_by_hop) with
//!    `keep_upgrade = true` for these requests;
//! 2. the upstream answer decides the outcome:
//!    * `101 Switching Protocols` — the accept headers are relayed and the two
//!      sockets are spliced with [`tokio::io::copy_bidirectional`], so frames
//!      flow both ways without the gateway interpreting them;
//!    * anything else — status, headers and body are relayed like any other
//!      proxied response.
//!
//! No WebSocket framing is parsed here: once the handshake has been relayed the
//! gateway is a byte pipe, which is what a proxy has to be to stay transparent
//! for extensions (`permessage-deflate` included).

use std::time::Duration;

use axum::body::Body;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use hyper_util::rt::TokioIo;

use crate::domain::error::DomainError;
use crate::domain::model::Endpoint;
use crate::infra::proxy::headers as h;
use crate::infra::proxy::transport::{ProxyTransport, TransportError};

/// Response status of a successful handshake.
const SWITCHING_PROTOCOLS: u16 = 101;

/// Response headers relayed verbatim from the upstream `101`.
const RELAYED_UPGRADE_HEADERS: [&str; 2] = ["sec-websocket-accept", "sec-websocket-protocol"];

/// The hop-by-hop headers a handshake *needs*.
pub const HANDSHAKE_HEADERS: [&str; 2] = ["connection", "upgrade"];

/// True when the request asks for a protocol upgrade.
///
/// A request is an upgrade when `Connection` announces the `upgrade` token and
/// `Upgrade` names a protocol (RFC 9110 §7.6.1, RFC 6455 §4.1); a bare
/// `Upgrade` header alone is not an upgrade.
#[must_use]
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = h::header_values(headers, "upgrade");
    if upgrade.is_empty() {
        return false;
    }
    // RFC 9110 §7.6.1: `Connection` lists *field names*, and an upgrade request
    // announces itself as `Connection: upgrade`. The `Upgrade` header names the
    // protocol (`websocket`, `h2c`, …) and never appears in that list.
    let announced = h::header_values(headers, "connection")
        .join(", ")
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !announced {
        return false;
    }
    upgrade
        .iter()
        .any(|value| value.split(',').any(|protocol| !protocol.trim().is_empty()))
}

/// Relay an `Upgrade` handshake and tunnel the result.
///
/// `outbound` carries the method, URI and handshake headers the pipeline built
/// (its body is empty); `inbound` is consumed because its
/// [`hyper::upgrade::OnUpgrade`] extension is the client side of the tunnel.
///
/// # Errors
/// [`DomainError`] for a refused dial or a transport failure. A *rejected*
/// handshake is not an error: the upstream answer is relayed as-is.
pub async fn tunnel(
    transport: &ProxyTransport,
    inbound: Request<Body>,
    outbound: Request<Body>,
    endpoint: &Endpoint,
    timeout: Duration,
) -> Result<axum::response::Response, DomainError> {
    // The client side has to be claimed before the request is handed back to
    // hyper, so take it up front and pass the future on.
    let mut inbound = inbound;
    let client_side = hyper::upgrade::on(&mut inbound);
    let _ = inbound;

    let upstream = match transport.send_within(outbound, timeout).await {
        Ok(upstream) => upstream,
        Err(err) => {
            // No handshake ever happened; the inbound upgrade is dropped with
            // the request, which closes the client connection.
            drop(client_side);
            return Err(upstream_error(err, endpoint));
        }
    };

    if upstream.status().as_u16() != SWITCHING_PROTOCOLS {
        return relay_rejection(upstream);
    }

    // Relay the accept headers, then claim the upstream side of the tunnel.
    let mut upstream = upstream;
    let mut relayed = HeaderMap::new();
    for name in RELAYED_UPGRADE_HEADERS {
        for value in upstream.headers().get_all(name) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                relayed.append(name, value);
            }
        }
    }
    for name in HANDSHAKE_HEADERS {
        for value in upstream.headers().get_all(name) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                relayed.insert(name, value);
            }
        }
    }
    let upstream_side = hyper::upgrade::on(&mut upstream);
    drop(upstream);

    let mut response = Response::builder()
        .status(http::StatusCode::from_u16(SWITCHING_PROTOCOLS).unwrap_or(http::StatusCode::OK))
        .body(Body::empty())
        .map_err(|err| DomainError::ProtocolError {
            detail: format!("unable to build the switching-protocols response: {err}"),
        })?;
    *response.headers_mut() = relayed;

    // The response is returned immediately so the client can finish its
    // handshake; the sockets are spliced in the background.
    tokio::spawn(async move {
        let (client, upstream) = (client_side.await, upstream_side.await);
        match (client, upstream) {
            (Ok(client), Ok(upstream)) => {
                let mut client = TokioIo::new(client);
                let mut upstream = TokioIo::new(upstream);
                if let Err(err) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                    tracing::debug!(error = %err, "upgrade tunnel closed");
                }
            }
            (Err(err), _) | (_, Err(err)) => {
                tracing::debug!(error = %err, "upgrade tunnel could not be established");
            }
        }
    });
    Ok(response)
}

/// Relay the upstream's answer to a refused (or ordinary) exchange.
fn relay_rejection(
    upstream: http::Response<hyper::body::Incoming>,
) -> Result<axum::response::Response, DomainError> {
    let (parts, body) = upstream.into_parts();
    let mut headers = parts.headers.clone();
    h::strip_hop_by_hop(&mut headers, false);
    let mut builder = axum::response::Response::builder().status(parts.status);
    for (name, value) in headers.iter() {
        builder = builder.header(name.as_str(), value.clone());
    }
    builder
        .body(Body::new(body))
        .map_err(|err| DomainError::ProtocolError {
            detail: format!("unable to relay the upgrade response: {err}"),
        })
}

/// Map a transport failure onto the domain error the client sees.
fn upstream_error(err: TransportError, endpoint: &Endpoint) -> DomainError {
    let host = endpoint.authority();
    match err {
        TransportError::Timeout { detail } => DomainError::ConnectionTimeout {
            detail,
            retry_after_seconds: None,
        },
        TransportError::Connect { detail } => DomainError::DownstreamError {
            detail: format!("upstream `{host}` is unreachable: {detail}"),
            upstream_id: None,
            host: Some(host),
            path: None,
        },
        TransportError::PlaintextDisabled { host } => DomainError::LinkUnavailable {
            detail: format!(
                "upstream `{host}` uses a cleartext scheme and `allow_http_upstream` is disabled"
            ),
            retry_after_seconds: None,
        },
        TransportError::InvalidUri { detail } => DomainError::ProtocolError { detail },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                name.parse::<HeaderName>().unwrap(),
                value.parse::<HeaderValue>().unwrap(),
            );
        }
        map
    }

    #[test]
    fn a_websocket_handshake_is_an_upgrade() {
        let headers = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        assert!(is_upgrade(&headers));
    }

    #[test]
    fn an_upgrade_header_without_connection_is_not() {
        let headers = headers(&[("upgrade", "websocket")]);
        assert!(!is_upgrade(&headers));
    }

    #[test]
    fn a_plain_request_is_not_an_upgrade() {
        let headers = headers(&[("connection", "keep-alive")]);
        assert!(!is_upgrade(&headers));
    }

    #[test]
    fn an_unrelated_protocol_is_still_an_upgrade() {
        let headers = headers(&[("connection", "upgrade"), ("upgrade", "h2c")]);
        assert!(is_upgrade(&headers));
    }
}
