//! Outbound proxy transport (DESIGN §3.2 "Proxy API", ADR-0007).
//!
//! The transport owns connection pooling, TLS, timeouts and the raw
//! WebSocket tunnel. It never inspects policy — that is `infra::proxy`'s job.

use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::error::{DomainError, DomainResult};

/// Headers defined hop-by-hop by RFC 9110 §7.6.1 and never forwarded.
pub const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// Names listed in an inbound `Connection` header, which are hop-by-hop too.
#[must_use]
pub fn connection_named(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(reqwest::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// `true` when `name` must never cross a proxy hop.
#[must_use]
pub fn is_hop_by_hop(name: &str, extra: &[String]) -> bool {
    let lowered = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lowered.as_str())
        || extra
            .iter()
            .any(|listed| listed.to_ascii_lowercase() == lowered)
}

/// `true` when the inbound request asks for a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(reqwest::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let upgrade = headers
        .get(reqwest::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    connection.split(',').any(|token| token.trim() == "upgrade") && upgrade == "websocket"
}

/// An outbound request the transport dials.
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// Absolute URL including scheme, host, port and path.
    pub url: String,
    /// HTTP method.
    pub method: http::Method,
    /// Headers already sanitised by the proxy engine.
    pub headers: HeaderMap,
    /// Fully buffered body (bounded by the gear body limit).
    pub body: Option<Bytes>,
    /// Wall-clock budget for the whole hop.
    pub timeout: Duration,
}

/// An upstream response whose body streams.
#[derive(Debug)]
pub struct OutboundResponse {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Upstream headers, sanitised by the proxy engine.
    pub headers: HeaderMap,
    /// Streaming body.
    pub inner: reqwest::Response,
}

impl OutboundResponse {
    /// Status as a plain `u16`.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        self.status.as_u16()
    }
}

/// Pooling HTTP(S) client for the data plane.
#[derive(Clone)]
pub struct OutboundClient {
    client: Client,
}

impl std::fmt::Debug for OutboundClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OutboundClient").finish()
    }
}

impl OutboundClient {
    /// Builds a client with the gear's timeout posture.
    ///
    /// No client-wide deadline is installed: reqwest's total timeout spans
    /// connect *through the end of the body*, which would sever every
    /// server-sent-event stream at `proxy_timeout_secs`. The response-header
    /// budget is applied per request in [`OutboundClient::send`], and the body
    /// is bounded by the gear's idle timeout instead (DESIGN §3.2).
    ///
    /// Redirects are never followed automatically: an outbound gateway must
    /// hand the redirect back to the caller, and following one would silently
    /// bypass the configured endpoint and its SSRF checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the TLS backend cannot be initialised.
    pub fn new(
        _proxy_timeout: Duration,
        connect_timeout: Duration,
        pool_idle: Duration,
        pool_per_host: usize,
    ) -> DomainResult<Self> {
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(pool_idle)
            .pool_max_idle_per_host(pool_per_host)
            .no_proxy()
            .build()
            .map_err(|error| DomainError::LinkUnavailable(error.to_string()))?;
        Ok(Self { client })
    }

    /// Dials `request` and returns the streaming response.
    ///
    /// The deadline bounds the *exchange of headers*: `send()` resolves as soon
    /// as the upstream answers, so the body — which may be an unbounded event
    /// stream — is never cut off by it.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionTimeout` for a connect-phase timeout,
    /// `RequestTimeout` when the upstream is silent past the header budget, and
    /// `DownstreamError` for any other transport failure.
    pub async fn send(&self, request: OutboundRequest) -> DomainResult<OutboundResponse> {
        let builder = self
            .client
            .request(
                request.method.clone(),
                reqwest::Url::parse(&request.url).map_err(|error| {
                    DomainError::ProtocolError(format!("upstream URL is not dialable: {error}"))
                })?,
            )
            .headers(request.headers.clone());
        let builder = match request.body.clone() {
            Some(body) => builder.body(body),
            None => builder,
        };
        let response = match tokio::time::timeout(request.timeout, builder.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(transport_error(&error)),
            Err(_elapsed) => return Err(DomainError::RequestTimeout),
        };
        let status = response.status();
        Ok(OutboundResponse {
            status,
            headers: response.headers().clone(),
            inner: response,
        })
    }

    /// Opens a WebSocket tunnel: sends the upgrade request and returns the
    /// raw byte socket after the upstream's 101.
    ///
    /// # Errors
    ///
    /// Returns `RequestTimeout` when the handshake stalls, and `ProtocolError`
    /// when the upstream declines the upgrade.
    pub async fn connect_websocket(
        &self,
        url: String,
        headers: HeaderMap,
        timeout: Duration,
    ) -> DomainResult<reqwest::Upgraded> {
        let builder = self
            .client
            .request(
                http::Method::GET,
                reqwest::Url::parse(&url).map_err(|error| {
                    DomainError::ProtocolError(format!("upstream URL is not dialable: {error}"))
                })?,
            )
            .headers(headers);
        // The upgrade headers are set explicitly: the client-side handshake is
        // ours, the server-side (client-facing) one belongs to the axum layer.
        // The key is minted here because the caller's key was stripped with the
        // rest of the hop-by-hop material on the way in.
        let builder = builder
            .header(reqwest::header::CONNECTION, "Upgrade")
            .header(reqwest::header::UPGRADE, "websocket")
            .header("sec-websocket-version", "13")
            .header(
                "sec-websocket-key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            );
        let response = match tokio::time::timeout(timeout, builder.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(transport_error(&error)),
            Err(_elapsed) => return Err(DomainError::RequestTimeout),
        };
        // An upstream that declines names its own status, and the caller must
        // be able to attribute the failure to it (ADR-0007).
        if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(DomainError::UpstreamRejectedUpgrade {
                status: response.status().as_u16(),
            });
        }
        response
            .upgrade()
            .await
            .map_err(|error| DomainError::ProtocolError(format!("upgrade failed: {error}")))
    }
}

fn transport_error(error: &reqwest::Error) -> DomainError {
    // Order matters: a connect failure can also carry the timeout flag, and the
    // connect diagnosis is the more useful one. A failure while sending the
    // request (a reset mid-upload) is a broken hop, not a timeout, so it lands
    // on the 502 row rather than a retriable 504.
    if error.is_connect() {
        DomainError::LinkUnavailable(error.to_string())
    } else if error.is_timeout() {
        DomainError::ConnectionTimeout
    } else {
        DomainError::DownstreamError(error.to_string())
    }
}

/// Brackets an IPv6 literal so it is diallable in a URL authority.
#[must_use]
pub fn host_token(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// Renders headers onto a [`HeaderMap`], skipping values the protocol cannot
/// carry.
pub fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    } else {
        tracing::debug!(header = %name, "dropping a header value the wire cannot carry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_never_forwarded() {
        for name in HOP_BY_HOP {
            assert!(is_hop_by_hop(name, &[]), "{name} is hop-by-hop");
        }
        assert!(!is_hop_by_hop("x-request-id", &[]));
        assert!(is_hop_by_hop("x-drop", &["X-Drop".to_owned()]));
    }

    #[test]
    fn connection_headers_are_collected() {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "connection", "Keep-Alive, X-Internal");
        let named = connection_named(&headers);
        assert!(named.contains(&"keep-alive".to_owned()));
        assert!(named.contains(&"x-internal".to_owned()));
        assert!(is_hop_by_hop("x-internal", &named));
    }

    #[test]
    fn websocket_upgrades_are_detected() {
        let mut headers = HeaderMap::new();
        assert!(!is_websocket_upgrade(&headers));
        insert_header(&mut headers, "connection", "Upgrade");
        insert_header(&mut headers, "upgrade", "websocket");
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn ipv6_literal_hosts_are_bracketed() {
        assert_eq!(host_token("2606:4700::1111"), "[2606:4700::1111]");
        assert_eq!(host_token("[2606:4700::1111]"), "[2606:4700::1111]");
        assert_eq!(host_token("api.example.com"), "api.example.com");
    }

    #[test]
    fn unparsable_headers_are_dropped_silently() {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "x-good", "value");
        insert_header(&mut headers, "x-bad", "bad\nvalue");
        // A space is not a valid header-name token (RFC 9110 `tchar`).
        insert_header(&mut headers, "bad name", "value");
        assert_eq!(headers.len(), 1);
        assert!(headers.get("x-good").is_some());
    }

    #[tokio::test]
    async fn client_refuses_an_unparsable_url() {
        let client = OutboundClient::new(
            Duration::from_secs(2),
            Duration::from_secs(1),
            Duration::from_secs(30),
            32,
        )
        .expect("client");
        let error = client
            .send(OutboundRequest {
                url: "not a url".to_owned(),
                method: http::Method::GET,
                headers: HeaderMap::new(),
                body: None,
                timeout: Duration::from_secs(1),
            })
            .await
            .expect_err("protocol error");
        assert_eq!(error.status_code(), 502);
    }
}
