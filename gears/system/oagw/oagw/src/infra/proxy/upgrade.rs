//! WebSocket upgrade passthrough.
//!
//! The gateway terminates the client's HTTP/1.1 upgrade with its own
//! `Sec-WebSocket-Accept` and then splices raw bytes between the upgraded
//! client connection and the upgraded upstream connection, so any WebSocket
//! subprotocol and framing works unchanged (DESIGN "Proxy API").
//!
//! Server-side mechanics: hyper exposes the connection's upgrade handle as a
//! `hyper::upgrade::OnUpgrade` in the **request** extensions. Awaiting it
//! yields the raw upgraded client stream once the 101 response is written.

use http::{HeaderMap, HeaderValue, StatusCode};
use std::pin::Pin;

use crate::domain::error::{OagwError, OagwResult};

/// Header carrying the WebSocket client key.
pub const SEC_WEBSOCKET_KEY: &str = "sec-websocket-key";

/// Header carrying the negotiated subprotocol.
pub const SEC_WEBSOCKET_PROTOCOL: &str = "sec-websocket-protocol";

/// Computes `Sec-WebSocket-Accept` for a client `Sec-WebSocket-Key`.
#[must_use]
pub fn websocket_accept(key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};

    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut hasher = Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(GUID.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Builds the 101 response for the client upgrade.
///
/// The caller must return this response to axum; the client-side upgraded
/// stream comes from the `OnUpgrade` handle taken out of the request
/// extensions.
///
/// # Errors
///
/// [`OagwError::Validation`] when the request carries no usable
/// `Sec-WebSocket-Key`.
pub fn switch_to_websocket(
    client_headers: &HeaderMap,
    selected_protocol: Option<&str>,
) -> OagwResult<axum::response::Response> {
    let key = client_headers
        .get(SEC_WEBSOCKET_KEY)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            OagwError::Validation("websocket upgrade requires Sec-WebSocket-Key".to_owned())
        })?;
    let accept = websocket_accept(key);
    let mut builder = axum::http::Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", accept);
    if let Some(protocol) = selected_protocol {
        builder = builder.header(SEC_WEBSOCKET_PROTOCOL, protocol);
    }
    let response = builder
        .body(axum::body::Body::empty())
        .map_err(|err| OagwError::Internal(format!("failed to build upgrade response: {err}")))?;
    Ok(response)
}

/// Bridges hyper's runtime-agnostic IO traits onto tokio's so an upgraded
/// stream can be used with [`tokio::io::copy_bidirectional`].
struct UpgradedIo(hyper::upgrade::Upgraded);

impl tokio::io::AsyncRead for UpgradedIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Initialize tokio's unfilled tail so hyper can read into it directly;
        // hyper's `ReadBuf` assumes its backing slice is initialized.
        let filled = {
            let slice = buf.initialize_unfilled();
            let mut hyper_buf = hyper::rt::ReadBuf::new(slice);
            match hyper::rt::Read::poll_read(Pin::new(&mut self.0), cx, hyper_buf.unfilled()) {
                std::task::Poll::Ready(Ok(())) => hyper_buf.filled().len(),
                std::task::Poll::Ready(Err(err)) => {
                    return std::task::Poll::Ready(Err(err));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        };
        buf.advance(filled);
        std::task::Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for UpgradedIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        hyper::rt::Write::poll_write(Pin::new(&mut self.0), cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        hyper::rt::Write::poll_flush(Pin::new(&mut self.0), cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        hyper::rt::Write::poll_shutdown(Pin::new(&mut self.0), cx)
    }
}

/// Splices an upgraded inbound connection with an upgraded upstream one.
///
/// Runs in the background: the `101 Switching Protocols` response has already
/// been written, so the caller must not block on the copy finishing.
pub fn splice_upgraded(
    client_upgrade: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::Upgraded,
) {
    tokio::spawn(async move {
        let client = match client_upgrade.await {
            Ok(client) => UpgradedIo(client),
            Err(err) => {
                tracing::debug!("client upgrade never completed: {err}");
                return;
            }
        };
        let mut client = client;
        let mut upstream = UpgradedIo(upstream);
        match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
            Ok((to_upstream, to_client)) => {
                tracing::debug!("upgraded stream closed: {to_upstream} bytes out, {to_client} in");
            }
            Err(err) => tracing::debug!("upgraded stream error: {err}"),
        }
    });
}

/// Upstream request headers for a WebSocket upgrade (HTTP/1.1 only).
#[must_use]
pub fn upstream_upgrade_headers(client_headers: &HeaderMap, host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in [
        SEC_WEBSOCKET_KEY,
        "sec-websocket-version",
        SEC_WEBSOCKET_PROTOCOL,
        "sec-websocket-extensions",
    ] {
        if let Some(value) = client_headers.get(name) {
            if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
                headers.insert(name, value.clone());
            }
        }
    }
    if let Ok(value) = HeaderValue::from_str(host) {
        headers.insert(http::header::HOST, value);
    }
    headers
}

/// Validates the caller's `X-OAGW-Target-Host` against the endpoints.
///
/// Returns the index of the matching endpoint, or `None` when no header was
/// supplied.
///
/// # Errors
///
/// [`OagwError::InvalidTargetHost`] for malformed values,
/// [`OagwError::UnknownTargetHost`] when no endpoint matches.
pub fn validate_target_host(
    endpoints: &[crate::domain::model::Endpoint],
    requested: Option<&str>,
) -> OagwResult<Option<usize>> {
    use crate::domain::alias;

    let Some(requested) = requested else {
        return Ok(None);
    };
    let normalized = alias::normalize(requested);
    if normalized.is_empty()
        || normalized.contains('/')
        || normalized.starts_with(':')
        || normalized.ends_with(':')
    {
        return Err(OagwError::InvalidTargetHost);
    }
    let bare_host = normalized.split(':').next().unwrap_or_default().to_owned();
    if bare_host.is_empty() {
        return Err(OagwError::InvalidTargetHost);
    }
    if !alias::is_ip(&bare_host) && !alias::is_valid_alias(&bare_host) {
        return Err(OagwError::InvalidTargetHost);
    }
    let index = endpoints.iter().position(|endpoint| {
        let host = alias::normalize(&endpoint.host);
        host == bare_host || format!("{host}:{}", endpoint.port) == normalized
    });
    match index {
        Some(index) => Ok(Some(index)),
        None => Err(OagwError::UnknownTargetHost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme};

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn accept_matches_rfc6455_example() {
        // RFC 6455 §1.3 example.
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn accept_trims_surrounding_whitespace() {
        assert_eq!(
            websocket_accept("  dGhlIHNhbXBsZSBub25jZQ==  "),
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ==")
        );
    }

    #[test]
    fn target_host_without_header_is_auto() {
        let endpoints = vec![endpoint("api.example.com", 443)];
        assert_eq!(validate_target_host(&endpoints, None).unwrap(), None);
    }

    #[test]
    fn target_host_matches_endpoint_by_name() {
        let endpoints = vec![endpoint("a.example.com", 443), endpoint("b.example.com", 443)];
        assert_eq!(validate_target_host(&endpoints, Some("b.example.com")).unwrap(), Some(1));
        assert_eq!(
            validate_target_host(&endpoints, Some("B.Example.COM")).unwrap(),
            Some(1),
            "matching is case-insensitive and still yields the same endpoint"
        );
    }

    #[test]
    fn target_host_with_port_matches() {
        let endpoints = vec![endpoint("a.example.com", 8443)];
        assert_eq!(
            validate_target_host(&endpoints, Some("a.example.com:8443")).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn unknown_target_host_is_rejected() {
        let endpoints = vec![endpoint("a.example.com", 443)];
        assert!(matches!(
            validate_target_host(&endpoints, Some("other.example.com")),
            Err(crate::domain::error::OagwError::UnknownTargetHost)
        ));
    }

    #[test]
    fn invalid_target_host_is_rejected() {
        let endpoints = vec![endpoint("a.example.com", 443)];
        assert!(matches!(
            validate_target_host(&endpoints, Some("a.example.com/path")).unwrap_err(),
            crate::domain::error::OagwError::InvalidTargetHost
        ));
        assert!(matches!(
            validate_target_host(&endpoints, Some("")).unwrap_err(),
            crate::domain::error::OagwError::InvalidTargetHost
        ));
    }
}
