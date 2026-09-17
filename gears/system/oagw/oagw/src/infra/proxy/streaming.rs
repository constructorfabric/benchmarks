//! Streaming passthrough and protocol upgrades (PRD.md §5.4, §8).
//!
//! Server-sent events are never buffered: the upstream body is handed to the
//! response as an unrestricted stream, so every event reaches the caller as
//! it arrives. A request that asks for a protocol upgrade (`Connection:
//! Upgrade` + `Upgrade: websocket`) is *tunnelled* instead of proxied — the
//! gateway answers the caller with the upstream's `101` headers and then
//! splices the two sockets, so frames flow in both directions and the
//! lifecycle (open / close / error) is whatever the two ends negotiate.
//!
//! Upgrades are only tunnelled to plaintext `http` endpoints: the platform
//! transport has no way to hand back a TLS socket, so an `https` endpoint is
//! refused up front instead of half-opened (see [`TunnelError`]).

use std::time::Duration;

use axum::http::HeaderMap;
use hyper_util::rt::TokioIo;

/// Content type of a Server-Sent Events response (PRD.md §5.4).
pub const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// The `Upgrade` token of a WebSocket handshake.
pub const WEBSOCKET_PROTOCOL: &str = "websocket";

/// `101 Switching Protocols`, the answer both legs of a tunnel expect.
pub const SWITCHING_PROTOCOLS: u16 = 101;

/// How long the caller has to finish its side of an upgrade once the gateway
/// answered `101`.
const UPGRADE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a response is an unbounded stream the gateway must not buffer.
///
/// Everything else is passed through streaming too; this marks the responses
/// whose *end* is not the end of the message (events keep arriving until the
/// upstream closes).
#[must_use]
pub fn is_streamed(status: u16, content_type: Option<&str>) -> bool {
    if status == SWITCHING_PROTOCOLS {
        return true;
    }
    let Some(media_type) = content_type else {
        return false;
    };
    let media_type = media_type.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case(SSE_CONTENT_TYPE)
}

/// The protocol a request asks to upgrade to, if any.
///
/// A request *tunnels* when it carries `Connection: Upgrade` and an `Upgrade`
/// token; the hop-by-hop stripping of the plain proxy path does not apply to
/// it (DESIGN.md §3.4).
#[must_use]
pub fn upgrade_protocol(headers: &HeaderMap) -> Option<String> {
    let connection = headers.get(axum::http::header::CONNECTION)?.to_str().ok()?;
    let wants_upgrade = connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !wants_upgrade {
        return None;
    }
    let upgrade = headers.get(axum::http::header::UPGRADE)?.to_str().ok()?;
    upgrade
        .split(',')
        .map(str::trim)
        .find(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
}

/// The upstream leg of an established upgrade.
#[derive(Debug)]
pub struct UpgradedUpstream {
    /// Status the upstream answered with (`101 Switching Protocols`).
    pub status: u16,
    /// Response headers of the handshake, passed through verbatim.
    pub headers: Vec<(String, String)>,
    /// The duplex stream of the upgraded connection.
    pub io: TokioIo<hyper::upgrade::Upgraded>,
}

/// A request to tunnel to an upstream.
#[derive(Debug, Clone)]
pub struct TunnelRequest {
    /// HTTP method of the handshake (`GET` for a WebSocket).
    pub method: String,
    /// Absolute URL of the upstream endpoint.
    pub url: String,
    /// Outbound headers, `Connection`/`Upgrade` included.
    pub headers: Vec<(String, String)>,
}

/// Why an upgrade could not be tunnelled.
#[derive(Debug)]
pub enum TunnelError {
    /// The endpoint URL is not a usable upstream target.
    InvalidTarget(String),
    /// The endpoint is not plaintext: upgrades need a real socket.
    UnsupportedScheme(String),
    /// The upstream answered, but not with `101 Switching Protocols`.
    Refused {
        /// Status the upstream answered with.
        status: u16,
        /// Headers the upstream answered with.
        headers: Vec<(String, String)>,
    },
    /// The handshake or the upgrade itself failed.
    Handshake(String),
    /// The endpoint did not answer within the configured timeout.
    Timeout(Duration),
}

impl TunnelError {
    /// The rendered gateway error of a failed tunnel.
    #[must_use]
    pub fn into_error(self) -> crate::domain::error::DomainError {
        use crate::domain::error::{DomainError, ErrorKind};
        use serde_json::json;
        match self {
            Self::UnsupportedScheme(scheme) => DomainError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "protocol upgrades are only supported to `http` upstream endpoints, not to \
                     `{scheme}`"
                ),
            ),
            Self::Refused { status, headers } => DomainError::new(
                ErrorKind::ProtocolError,
                format!("the upstream refused the protocol upgrade with status {status}"),
            )
            .with_field("upstream_status", json!(status))
            .with_field("upstream_headers", json!(headers)),
            Self::Timeout(timeout) => DomainError::new(
                ErrorKind::RequestTimeout,
                format!("the upstream did not answer the upgrade within {timeout:?}"),
            ),
            Self::InvalidTarget(url) => DomainError::new(
                ErrorKind::ProtocolError,
                format!("the upgrade target {url:?} is not a usable upstream endpoint"),
            ),
            Self::Handshake(detail) => DomainError::new(
                ErrorKind::ProtocolError,
                format!("the upgrade handshake failed: {detail}"),
            ),
        }
    }
}

/// The gateway error of a failed tunnel, with the endpoint stamped on it.
#[must_use]
pub fn tunnel_error(error: TunnelError, endpoint: &str) -> crate::domain::error::DomainError {
    let mut rendered = error.into_error();
    rendered
        .fields
        .push(("endpoint", serde_json::json!(endpoint)));
    rendered
}

/// Opens the upstream leg of a protocol upgrade.
///
/// The whole handshake — connect, request and `101` — is bounded by
/// `timeout`; the tunnel itself is unbounded once established.
///
/// # Errors
/// Returns a [`TunnelError`] for a plaintext-only policy violation, a refused
/// upgrade, a handshake failure or a timeout.
pub async fn open_tunnel(
    request: TunnelRequest,
    timeout: Duration,
) -> Result<UpgradedUpstream, TunnelError> {
    let uri: hyper::Uri = request
        .url
        .parse()
        .map_err(|_| TunnelError::InvalidTarget(request.url.clone()))?;
    let scheme = uri.scheme_str().unwrap_or_default().to_ascii_lowercase();
    if scheme != "http" {
        return Err(TunnelError::UnsupportedScheme(scheme));
    }
    let host = uri
        .host()
        .map(str::to_owned)
        .ok_or_else(|| TunnelError::InvalidTarget(request.url.clone()))?;
    let port = uri.port_u16().unwrap_or(80);
    let connect = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| TunnelError::Timeout(timeout))?
    .map_err(|error| TunnelError::Handshake(format!("connect to {host}:{port}: {error}")))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(connect))
        .await
        .map_err(|error| {
            TunnelError::Handshake(format!("handshake with {host}:{port}: {error}"))
        })?;
    tokio::spawn(async move {
        // `with_upgrades` is what makes the client leg deliver the socket: a
        // bare `Connection` would hand the upgrade back as "handled manually".
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(error = %error, "upgrade connection ended");
        }
    });
    let mut builder = hyper::Request::builder()
        .method(request.method.as_str())
        .uri(uri);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let outbound = builder
        .body(axum::body::Body::empty())
        .map_err(|error| TunnelError::Handshake(error.to_string()))?;
    let response = tokio::time::timeout(timeout, sender.send_request(outbound))
        .await
        .map_err(|_| TunnelError::Timeout(timeout))?
        .map_err(|error| TunnelError::Handshake(error.to_string()))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    if status != SWITCHING_PROTOCOLS {
        return Err(TunnelError::Refused { status, headers });
    }
    let io = TokioIo::new(
        hyper::upgrade::on(response)
            .await
            .map_err(|error| TunnelError::Handshake(error.to_string()))?,
    );
    Ok(UpgradedUpstream {
        status,
        headers,
        io,
    })
}

/// Splices an established upstream leg onto the caller's pending upgrade.
///
/// Runs until either end closes; both directions are reported when it ends.
pub async fn splice(upstream: UpgradedUpstream, inbound: hyper::upgrade::OnUpgrade) {
    let mut inbound = match tokio::time::timeout(UPGRADE_HANDSHAKE_TIMEOUT, inbound).await {
        Ok(Ok(stream)) => TokioIo::new(stream),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "the caller's upgrade never completed");
            return;
        }
        Err(_) => {
            tracing::warn!("the caller's upgrade timed out before it completed");
            return;
        }
    };
    let mut upstream = upstream.io;
    match tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await {
        Ok((sent, received)) => tracing::debug!(sent, received, "upgrade tunnel closed"),
        Err(error) => tracing::warn!(error = %error, "upgrade tunnel failed"),
    }
}

/// The passthrough body of an upstream response.
///
/// Frames are forwarded as they arrive — no buffering, no size limit — which
/// is what makes SSE events reach the caller one by one (PRD.md §8).
pub fn passthrough(body: toolkit_http::ResponseBody) -> axum::body::Body {
    axum::body::Body::new(body)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sse_and_upgrades_are_streamed() {
        assert!(is_streamed(SWITCHING_PROTOCOLS, Some("application/json")));
        assert!(is_streamed(200, Some("text/event-stream")));
        assert!(is_streamed(200, Some("text/event-stream; charset=utf-8")));
        assert!(!is_streamed(200, Some("application/json")));
        assert!(!is_streamed(200, Some("text/html; charset=utf-8")));
        assert!(!is_streamed(200, None));
    }

    #[test]
    fn a_connection_upgrade_is_recognised() {
        let mut headers = HeaderMap::new();
        assert!(upgrade_protocol(&headers).is_none());
        headers.insert(
            axum::http::header::CONNECTION,
            "keep-alive".parse().unwrap(),
        );
        headers.insert(axum::http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(
            upgrade_protocol(&headers).is_none(),
            "no `Connection: Upgrade`"
        );
        headers.insert(axum::http::header::CONNECTION, "Upgrade".parse().unwrap());
        assert_eq!(
            upgrade_protocol(&headers).as_deref(),
            Some(WEBSOCKET_PROTOCOL)
        );
        headers.insert(
            axum::http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(axum::http::header::UPGRADE, "WebSocket".parse().unwrap());
        assert_eq!(
            upgrade_protocol(&headers).as_deref(),
            Some(WEBSOCKET_PROTOCOL)
        );
    }

    #[tokio::test]
    async fn an_https_endpoint_is_refused_without_dialling() {
        let plan = TunnelRequest {
            method: "GET".to_owned(),
            url: "https://api.openai.com/v1/ws".to_owned(),
            headers: Vec::new(),
        };
        let error = open_tunnel(plan, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(matches!(error, TunnelError::UnsupportedScheme(ref scheme) if scheme == "https"));
        let rendered = tunnel_error(error, "api.openai.com:443");
        assert_eq!(rendered.status(), 503);
        assert!(rendered.detail.contains("https"));
        assert_eq!(
            rendered.field("endpoint"),
            Some(&serde_json::json!("api.openai.com:443"))
        );
    }

    #[tokio::test]
    async fn an_unparsable_target_is_invalid() {
        let plan = TunnelRequest {
            method: "GET".to_owned(),
            url: "not a url".to_owned(),
            headers: Vec::new(),
        };
        let error = open_tunnel(plan, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(matches!(error, TunnelError::InvalidTarget(_)));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_fails_the_handshake() {
        // Port 9 is the discard service: reserved on the loopback, so the
        // connect is refused instead of hanging until the timeout.
        let plan = TunnelRequest {
            method: "GET".to_owned(),
            url: "http://127.0.0.1:9/ws".to_owned(),
            headers: Vec::new(),
        };
        let started = std::time::Instant::now();
        let error = open_tunnel(plan, Duration::from_millis(250))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TunnelError::Handshake(_)),
            "refused by the loopback: {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_refused_upgrade_reports_the_upstream_status() {
        let error = TunnelError::Refused {
            status: 400,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
        };
        let rendered = error.into_error();
        assert_eq!(rendered.status(), 502);
        assert_eq!(
            rendered.field("upstream_status"),
            Some(&serde_json::json!(400))
        );
    }
}
