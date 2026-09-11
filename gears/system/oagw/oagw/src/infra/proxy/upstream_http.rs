//! Outbound HTTP: connect, send, and stream the response back.
//!
//! The connector is Pingora's (`cpt-cf-oagw-tech-dependencies`), which brings
//! connection pooling, TLS and ALPN negotiation. Two properties matter for the
//! contract:
//!
//! * **The response body is streamed, not buffered.** SSE works because the
//!   body is a lazy stream driven by the client's reads
//!   (`cpt-cf-oagw-fr-streaming`), so an event reaches the caller as soon as
//!   the upstream flushes it.
//! * **No automatic retries.** `cpt-cf-oagw-principle-no-retry`: the original
//!   client request is never re-issued. Connection-level failover inside the
//!   connector is permitted; re-sending the request is not.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use pingora_core::connectors::http::Connector;
use pingora_core::protocols::http::client::HttpSession;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_core::{Error as PingoraError, ErrorType};
use pingora_http::RequestHeader;

use crate::config::OagwConfig;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::Endpoint;

/// The request as it goes on the wire to the upstream.
#[derive(Debug)]
pub struct UpstreamRequest {
    /// Outbound method.
    pub method: Method,
    /// Path plus query string, already percent-encoded.
    pub path_and_query: String,
    /// Outbound headers, already transformed.
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
}

/// The response headers, plus the still-open session its body streams from.
pub struct UpstreamResponse {
    /// Upstream status.
    pub status: StatusCode,
    /// Upstream headers.
    pub headers: HeaderMap,
    session: Box<HttpSession>,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl UpstreamResponse {
    /// Turn the open session into a streaming Axum body.
    ///
    /// The stream is polled by the client's reads, so nothing is buffered and
    /// back-pressure propagates end to end. When the client disconnects the
    /// stream is dropped, which closes the upstream session — the SSE
    /// "client disconnects → close the upstream connection" behaviour.
    pub fn into_body(self) -> Body {
        let stream = futures_util::stream::unfold(Some(self.session), |state| async move {
            let mut session = state?;
            match session.read_response_body().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(session))),
                Ok(None) => None,
                Err(err) => {
                    let mapped: Result<Bytes, std::io::Error> = Err(std::io::Error::other(
                        format!("upstream stream aborted: {err}"),
                    ));
                    // Drop the session: the stream is over either way.
                    Some((mapped, None))
                }
            }
        });
        Body::from_stream(stream)
    }
}

/// Outbound HTTP client.
pub struct HttpForwarder {
    connector: Connector,
    config: Arc<OagwConfig>,
}

impl std::fmt::Debug for HttpForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpForwarder")
    }
}

impl HttpForwarder {
    /// Build a forwarder with a fresh connection pool.
    #[must_use]
    pub fn new(config: Arc<OagwConfig>) -> Self {
        Self {
            connector: Connector::new(None),
            config,
        }
    }

    /// Build the Pingora peer for an endpoint that has already been resolved
    /// and cleared by the SSRF guard.
    ///
    /// The peer is addressed by IP, so no further name resolution happens
    /// inside the connector and the address the guard approved is the address
    /// dialled. The SNI / `Host` value stays the configured hostname.
    #[must_use]
    pub fn build_peer(&self, endpoint: &Endpoint, addr: std::net::SocketAddr) -> Box<HttpPeer> {
        let tls = endpoint.scheme.is_tls();
        let sni = if tls {
            endpoint.normalized_host()
        } else {
            String::new()
        };
        let mut peer = HttpPeer::new(addr, tls, sni);
        let options = &mut peer.options;
        options.connection_timeout = Some(self.config.connect_timeout());
        options.total_connection_timeout = Some(self.config.connect_timeout());
        options.read_timeout = Some(self.config.idle_timeout());
        options.write_timeout = Some(self.config.proxy_timeout());
        // Adaptive version negotiation over TLS (`DESIGN.md` § *HTTP Version
        // Negotiation*): offer h2 and h1 via ALPN and let the connector cache
        // whichever the peer accepted. Plaintext has no negotiation mechanism.
        if tls {
            options.alpn = ALPN::H2H1;
        }
        Box::new(peer)
    }

    /// Send a request and read the response headers.
    ///
    /// # Errors
    ///
    /// * `502 DownstreamError` — connect or protocol failure.
    /// * `504 ConnectionTimeout` / `RequestTimeout` — deadline exceeded.
    pub async fn send(
        &self,
        peer: &HttpPeer,
        request: UpstreamRequest,
    ) -> OagwResult<UpstreamResponse> {
        let deadline = self.config.proxy_timeout();
        tokio::time::timeout(deadline, self.send_inner(peer, request))
            .await
            .map_err(|_| {
                OagwError::new(
                    ErrorKind::RequestTimeout,
                    format!("upstream did not respond within {}s", deadline.as_secs()),
                )
                .with_retry_after(deadline.as_secs().max(1))
            })?
    }

    async fn send_inner(
        &self,
        peer: &HttpPeer,
        request: UpstreamRequest,
    ) -> OagwResult<UpstreamResponse> {
        let (mut session, _reused) = self
            .connector
            .get_http_session(peer)
            .await
            .map_err(|err| map_pingora_error(&err, "could not connect to upstream"))?;

        session.set_read_timeout(Some(self.config.idle_timeout()));
        session.set_write_timeout(Some(self.config.proxy_timeout()));

        let mut header = RequestHeader::build(
            request.method.clone(),
            request.path_and_query.as_bytes(),
            Some(request.headers.len() + 2),
        )
        .map_err(|err| map_pingora_error(&err, "could not build the upstream request"))?;
        for (name, value) in &request.headers {
            header
                .append_header(name.clone(), value.clone())
                .map_err(|err| map_pingora_error(&err, "could not set an upstream header"))?;
        }
        // The body is buffered, so its length is known: send an explicit
        // Content-Length rather than chunked encoding.
        if !request.body.is_empty() || body_expected(&request.method) {
            header
                .insert_header("content-length", request.body.len().to_string())
                .map_err(|err| map_pingora_error(&err, "could not set content-length"))?;
        }

        session
            .write_request_header(Box::new(header))
            .await
            .map_err(|err| map_pingora_error(&err, "could not send the upstream request"))?;
        if !request.body.is_empty() {
            session
                .write_request_body(request.body, true)
                .await
                .map_err(|err| map_pingora_error(&err, "could not send the request body"))?;
        }
        session
            .finish_request_body()
            .await
            .map_err(|err| map_pingora_error(&err, "could not finish the request body"))?;

        session
            .read_response_header()
            .await
            .map_err(|err| map_pingora_error(&err, "could not read the upstream response"))?;
        let response_header = session.response_header().ok_or_else(|| {
            OagwError::new(
                ErrorKind::ProtocolError,
                "upstream returned no response header",
            )
        })?;
        let status = response_header.status;
        let headers = response_header.headers.clone();

        Ok(UpstreamResponse {
            status,
            headers,
            session: Box::new(session),
        })
    }
}

/// Whether a method conventionally carries a body, so an explicit
/// `Content-Length: 0` is worth sending.
fn body_expected(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH)
}

/// Map a Pingora error onto the documented OAGW error catalogue.
fn map_pingora_error(err: &PingoraError, context: &str) -> OagwError {
    let detail = format!("{context}: {err}");
    match err.etype() {
        ErrorType::ConnectTimedout | ErrorType::TLSHandshakeTimedout => {
            OagwError::new(ErrorKind::ConnectionTimeout, detail).with_retry_after(1)
        }
        ErrorType::ReadTimedout | ErrorType::WriteTimedout => {
            OagwError::new(ErrorKind::RequestTimeout, detail).with_retry_after(1)
        }
        ErrorType::InvalidHTTPHeader
        | ErrorType::H1Error
        | ErrorType::H2Error
        | ErrorType::InvalidH2
        | ErrorType::H2Downgrade => OagwError::new(ErrorKind::ProtocolError, detail),
        ErrorType::ConnectionClosed | ErrorType::ReadError | ErrorType::WriteError => {
            OagwError::new(ErrorKind::StreamAborted, detail)
        }
        _ => OagwError::new(ErrorKind::DownstreamError, detail),
    }
}

/// Build the `path?query` target for an outbound request.
#[must_use]
pub fn build_path_and_query(path: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return path.to_owned();
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in query {
        serializer.append_pair(key, value);
    }
    format!("{path}?{}", serializer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;
    use std::time::Duration;

    #[test]
    fn query_is_appended_and_encoded() {
        assert_eq!(build_path_and_query("/v1/chat", &[]), "/v1/chat");
        assert_eq!(
            build_path_and_query("/v1/chat", &[("model".to_owned(), "gpt 4".to_owned())]),
            "/v1/chat?model=gpt+4"
        );
        assert_eq!(
            build_path_and_query(
                "/v1/chat",
                &[
                    ("a".to_owned(), "1".to_owned()),
                    ("b".to_owned(), "&=".to_owned())
                ]
            ),
            "/v1/chat?a=1&b=%26%3D"
        );
    }

    #[test]
    fn body_is_expected_for_write_methods() {
        assert!(body_expected(&Method::POST));
        assert!(body_expected(&Method::PUT));
        assert!(body_expected(&Method::PATCH));
        assert!(!body_expected(&Method::GET));
        assert!(!body_expected(&Method::DELETE));
    }

    #[test]
    fn pingora_errors_map_to_the_documented_statuses() {
        let timeout = PingoraError::new(ErrorType::ConnectTimedout);
        assert_eq!(
            map_pingora_error(&timeout, "x").status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        let refused = PingoraError::new(ErrorType::ConnectRefused);
        assert_eq!(
            map_pingora_error(&refused, "x").status(),
            StatusCode::BAD_GATEWAY
        );
        let protocol = PingoraError::new(ErrorType::H1Error);
        assert_eq!(
            map_pingora_error(&protocol, "x").kind(),
            ErrorKind::ProtocolError
        );
        let closed = PingoraError::new(ErrorType::ConnectionClosed);
        assert_eq!(
            map_pingora_error(&closed, "x").kind(),
            ErrorKind::StreamAborted
        );
    }

    #[test]
    fn peers_carry_the_configured_deadlines_and_alpn() {
        let config = Arc::new(OagwConfig {
            connect_timeout_secs: 3,
            proxy_timeout_secs: 7,
            idle_timeout_secs: 11,
            ..OagwConfig::default()
        });
        let forwarder = HttpForwarder::new(Arc::clone(&config));

        let plaintext = Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: Some(8080),
        };
        let peer = forwarder.build_peer(&plaintext, "127.0.0.1:8080".parse().unwrap());
        assert!(!peer.is_tls());
        assert_eq!(
            peer.options.connection_timeout,
            Some(Duration::from_secs(3))
        );
        assert_eq!(peer.options.read_timeout, Some(Duration::from_secs(11)));
        assert_eq!(peer.options.write_timeout, Some(Duration::from_secs(7)));
        assert_eq!(peer.options.alpn, ALPN::H1, "no negotiation on plaintext");

        let tls = Endpoint {
            scheme: Scheme::Https,
            host: "API.OpenAI.com".to_owned(),
            port: Some(443),
        };
        let peer = forwarder.build_peer(&tls, "1.2.3.4:443".parse().unwrap());
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "api.openai.com", "SNI keeps the configured name");
        assert_eq!(peer.options.alpn, ALPN::H2H1);
    }
}
