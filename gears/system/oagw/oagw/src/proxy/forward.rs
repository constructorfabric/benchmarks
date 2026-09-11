//! Forward the Request to the Selected Endpoint
//! (`cpt-cf-oagw-algo-proxy-forward-request`).
//!
//! The only code path in this gear that opens a connection to an external
//! host (`cpt-cf-oagw-constraint-no-direct-internet`,
//! `inst-proxy-fw-sole-egress`).

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use toolkit_http::{HttpClient, HttpError, HttpResponse, RequestBuilder};

use crate::proxy::stream;

/// A `502`/`503`/`504` gateway-side forwarding failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForwardError {
    PlaintextRefused,
    ConnectionTimeout,
    RequestTimeout,
    IdleTimeout,
    LinkUnavailable,
    ProtocolError,
    /// Uncoded-condition assignment: the upstream response body could not
    /// be relayed intact (exceeded this path's own relay guard), mapped to
    /// the closest documented catalog entry.
    DownstreamBodyTooLarge,
}

/// The fully-buffered upstream response this non-streaming round relays.
#[derive(Debug, Clone)]
pub(crate) struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Status line and headers of an upstream response classified as an event
/// stream (`cpt-cf-oagw-algo-stream-sse-detect-relay`
/// `inst-stream-sse-detect-relay-02`): the body has deliberately **not**
/// been read at all, so the caller can commit and relay it incrementally
/// (`cpt-cf-oagw-principle-no-cache`).
#[derive(Debug)]
pub(crate) struct StreamingUpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub response: HttpResponse,
}

/// What [`forward_request`] produced once the upstream's response headers
/// arrived: a complete buffered message (proxy-core's, 2.5, original
/// behavior for every non-event-stream response), or a stream ready for
/// incremental relay (`cpt-cf-oagw-algo-stream-sse-detect-relay`).
#[derive(Debug)]
pub(crate) enum ForwardOutcome {
    Buffered(UpstreamResponse),
    Stream(StreamingUpstreamResponse),
}

fn start_request(client: &HttpClient, method: &Method, url: &str) -> Option<RequestBuilder> {
    let builder = match *method {
        Method::GET => client.get(url),
        Method::POST => client.post(url),
        Method::PUT => client.put(url),
        Method::PATCH => client.patch(url),
        Method::DELETE => client.delete(url),
        _ => return None,
    };
    Some(builder)
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Establish reachability of `host:port` within the remaining deadline,
/// giving a real "connection established" checkpoint the high-level HTTP
/// client's own single timeout otherwise does not expose
/// (`inst-proxy-fw-timeout-classify`'s connection-establishment phase).
async fn probe_connect(host: &str, port: u16, deadline: Instant) -> Result<(), ForwardError> {
    let lookup = tokio::time::timeout(remaining(deadline), tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| ForwardError::ConnectionTimeout)?
        .map_err(|_| ForwardError::LinkUnavailable)?;
    let mut addrs: Vec<_> = lookup.collect();
    if addrs.is_empty() {
        return Err(ForwardError::LinkUnavailable);
    }
    let addr = addrs.remove(0);
    match tokio::time::timeout(remaining(deadline), tokio::net::TcpStream::connect(addr)).await {
        Err(_) => Err(ForwardError::ConnectionTimeout),
        Ok(Err(_)) => Err(ForwardError::LinkUnavailable),
        Ok(Ok(_stream)) => Ok(()),
    }
}

/// Classify a transport-level [`HttpError`] surfaced while waiting for
/// response headers (`inst-proxy-fw-if-unavailable`, `inst-proxy-fw-if-protocol`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-protocol
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return-protocol
fn classify_send_error(error: &HttpError) -> ForwardError {
    match error {
        HttpError::InvalidUri { .. }
        | HttpError::InvalidScheme { .. }
        | HttpError::RequestBuild(_) => ForwardError::ProtocolError,
        HttpError::Transport(inner) => {
            let message = inner.to_string();
            if message.contains("connect")
                || message.contains("refused")
                || message.contains("reset")
            {
                ForwardError::LinkUnavailable
            } else if message.contains("parse") || message.contains("invalid") {
                // The upstream response could not be parsed as valid HTTP.
                ForwardError::ProtocolError
            } else {
                ForwardError::LinkUnavailable
            }
        }
        _ => ForwardError::LinkUnavailable,
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return-protocol
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-protocol
}

/// `cpt-cf-oagw-algo-proxy-forward-request`: open the connection under the
/// `proxy_timeout_secs` deadline and send the outbound request exactly
/// once. `host`/`port` are the selected endpoint's; `url` the fully
/// composed target.
// @cpt-algo:cpt-cf-oagw-algo-proxy-forward-request:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-forwarding:p1
// @cpt-dod:cpt-cf-oagw-dod-proxy-timeout-no-retry:p1
// @cpt-dod:cpt-cf-oagw-dod-proxy-plaintext-gate:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-deadline
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-send
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_request(
    client: &HttpClient,
    method: &Method,
    url: &str,
    host: &str,
    port: u16,
    headers: HeaderMap,
    body: Bytes,
    timeout_secs: u32,
) -> Result<ForwardOutcome, ForwardError> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));

    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-timeout
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-timeout-classify
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-unavailable
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return-unavailable
    probe_connect(host, port, deadline).await?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return-unavailable
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-unavailable
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-timeout-classify
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-timeout

    let Some(mut builder) = start_request(client, method, url) else {
        return Err(ForwardError::ProtocolError);
    };
    for (name, value) in &headers {
        if let Ok(value_str) = value.to_str() {
            builder = builder.header(name.as_str(), value_str);
        }
    }
    // DECOMPOSITION entry 2.6's WebSocket upgrade never reaches this
    // function at all: `toolkit_http::HttpClient` has no way to hand back a
    // raw, protocol-switched connection, so `proxy::engine` recognizes an
    // `Upgrade: websocket` request and diverts to `proxy::stream` (its own
    // `tokio_tungstenite` client handshake) before ever calling
    // `forward_request`. What *does* still run through this single call
    // site is the `text/event-stream` classification immediately below,
    // which is this hook's other half.
    let builder = builder.body_bytes(body);

    let send_result = tokio::time::timeout(remaining(deadline), builder.send()).await;
    let response = match send_result {
        Err(_elapsed) => return Err(ForwardError::RequestTimeout),
        Ok(Err(error)) => return Err(classify_send_error(&error)),
        Ok(Ok(response)) => response,
    };

    let status = response.status();
    let response_headers = response.headers().clone();
    let declared_len = response_headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-upgrade-hook
    // @cpt-algo:cpt-cf-oagw-algo-stream-sse-detect-relay:p2
    // @cpt-dod:cpt-cf-oagw-dod-stream-sse-detect-and-forward:p1
    // `cpt-cf-oagw-flow-stream-sse-consumption` step 3: only the status
    // line/headers of the response above have been read so far -- the body
    // is never touched before this classification decides how to relay it.
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-03
    // @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-01
    // @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-02
    // Classify the response by its `Content-Type` base media type, reading
    // only the status line and headers -- never the body -- before
    // deciding (`cpt-cf-oagw-algo-stream-sse-detect-relay` step 1-2). This
    // is the one call site every response (streaming or not) passes
    // through, so it is also where the `101`/event-stream branch entry 2.5
    // reserved for 2.6 actually lives.
    // @cpt-dod:cpt-cf-oagw-dod-stream-timeout-exemption:p1
    // `deadline`/`remaining(deadline)` (this function's `proxy_timeout_secs`
    // budget) is never consulted again once an event-stream outcome is
    // returned here: the caller (`stream::relay_event_stream`) reads the
    // rest of the body with no deadline of its own, satisfying
    // `cpt-cf-oagw-dod-stream-timeout-exemption` ("bounds only the
    // pre-commit phase -- connecting and receiving the initial response").
    if stream::is_event_stream(&response_headers) {
        return Ok(ForwardOutcome::Stream(StreamingUpstreamResponse {
            status,
            headers: response_headers,
            response,
        }));
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-02
    // @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-01
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-03
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-upgrade-hook

    // `cpt-cf-oagw-algo-stream-sse-detect-relay` step 3: reaching this point
    // at all means the classification above did not return the `Stream`
    // outcome, i.e. the base media type was not `text/event-stream` -- the
    // implicit "ELSE" of that `if`. The rest of this function is the
    // not-a-stream signal: it buffers and returns an ordinary
    // `ForwardOutcome::Buffered` for the caller's non-streaming path.
    // @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-07
    // @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-08
    let bytes_result = tokio::time::timeout(remaining(deadline), response.bytes()).await;
    let body = match bytes_result {
        Err(_elapsed) => return Err(ForwardError::IdleTimeout),
        Ok(Err(HttpError::BodyTooLarge { .. })) => {
            return Err(ForwardError::DownstreamBodyTooLarge);
        }
        Ok(Err(_error)) => return Err(ForwardError::ProtocolError),
        Ok(Ok(bytes)) => bytes,
    };

    // @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-if-truncated
    // @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-return-truncated
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-no-retry
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-no-breaker
    // Exactly one client-level attempt is made above: no retry loop, no
    // health tracking, no circuit breaker, per `cpt-cf-oagw-principle-no-retry`.
    if let Some(expected) = declared_len
        && expected != body.len()
    {
        return Err(ForwardError::DownstreamBodyTooLarge);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-no-breaker
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-no-retry
    // @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-return-truncated
    // @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-if-truncated

    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return
    Ok(ForwardOutcome::Buffered(UpstreamResponse {
        status,
        headers: response_headers,
        body,
    }))
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-return
    // @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-08
    // @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-07
}
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-send
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-deadline

/// Build the outbound target `{scheme}://{host}[:{port}]{path}[?{query}]`
/// (`inst-proxy-fw-compose`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-compose
pub(crate) fn compose_url(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    query: &[(String, String)],
) -> String {
    let mut url = format!("{scheme}://{host}:{port}{path}");
    if !query.is_empty() {
        let encoded: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        url.push('?');
        url.push_str(&encoded);
    }
    url
}
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-compose

/// Test-only: install the process-wide rustls crypto provider that
/// production reaches via `toolkit::bootstrap::init_crypto_provider`
/// before any gear is registered. This crate's own unit tests build an
/// [`HttpClient`] directly, without going through that bootstrap, so this
/// mirrors it here; the result is intentionally discarded because a
/// concurrently-run test may have already installed it.
#[cfg(test)]
fn ensure_test_crypto_provider() {
    let _ = toolkit::bootstrap::init_crypto_provider();
}

/// Build the [`HttpClient`] this path uses for all outbound requests:
/// retries disabled (`cpt-cf-oagw-principle-no-retry`), transparent
/// decompression left on (handled by stripping `Content-Encoding` on
/// relay), and a generous internal timeout because this path manages its
/// own deadline explicitly via [`forward_request`]'s outer
/// `tokio::time::timeout` calls.
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-sole-egress
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-version
pub(crate) fn build_http_client() -> HttpClient {
    #[cfg(test)]
    ensure_test_crypto_provider();

    // `forward_request` (the only caller of this client, keeping it the
    // sole egress path per `cpt-cf-oagw-constraint-no-direct-internet`)
    // negotiates HTTP/2 via ALPN and falls back to HTTP/1.1 transparently
    // through this client's underlying hyper-rustls connector; no
    // additional code in this gear does its own version negotiation.
    HttpClient::builder()
        .retry(None)
        .timeout(Duration::from_secs(3600))
        .max_body_size(crate::proxy::constants::MAX_BODY_BYTES)
        .transport(toolkit_http::TransportSecurity::AllowInsecureHttp)
        .build()
        .unwrap_or_else(|_| {
            HttpClient::new().expect("default toolkit-http client must always build")
        })
}
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-version
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-sole-egress

/// Deferred [`HttpClient`] construction: [`HttpClientBuilder::build`] spawns
/// a `tower::Buffer` worker task, which requires an active Tokio runtime.
/// Gear REST-route registration (`RestApiCapability::register_rest`) is not
/// guaranteed to run inside one, so this defers the actual build to the
/// first proxied request, which always runs inside the axum/tokio server
/// runtime.
#[derive(Default)]
pub(crate) struct LazyHttpClient(std::sync::OnceLock<HttpClient>);

impl std::fmt::Debug for LazyHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyHttpClient")
            .field("initialized", &self.0.get().is_some())
            .finish()
    }
}

impl LazyHttpClient {
    #[must_use]
    pub fn client(&self) -> &HttpClient {
        self.0.get_or_init(build_http_client)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn compose_url_without_query() {
        assert_eq!(
            compose_url("https", "example.com", 443, "/v1/x", &[]),
            "https://example.com:443/v1/x"
        );
    }

    #[test]
    fn compose_url_with_query() {
        let query = vec![("a".to_owned(), "1".to_owned())];
        assert_eq!(
            compose_url("http", "example.com", 80, "/v1", &query),
            "http://example.com:80/v1?a=1"
        );
    }

    #[tokio::test]
    async fn successful_forward_reaches_a_real_mock_server() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let _m = server.mock(|when, then| {
            when.method(GET).path("/x");
            then.status(200).body("hi");
        });
        let client = build_http_client();
        let url = format!("http://127.0.0.1:{}/x", server.port());
        let result = forward_request(
            &client,
            &axum::http::Method::GET,
            &url,
            "127.0.0.1",
            server.port(),
            HeaderMap::new(),
            Bytes::new(),
            2,
        )
        .await
        .unwrap();
        let ForwardOutcome::Buffered(result) = result else {
            panic!("a plain 200 JSON-less response must not be classified as an event stream");
        };
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.body, Bytes::from_static(b"hi"));
    }

    /// `cpt-cf-oagw-algo-stream-sse-detect-relay` step 2: a `text/event-stream`
    /// response (base media type only, ignoring `charset`) is classified as
    /// a stream and its body is left completely unread here.
    #[tokio::test]
    async fn event_stream_content_type_is_classified_as_a_stream_not_buffered() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let _m = server.mock(|when, then| {
            when.method(GET).path("/events");
            then.status(200)
                .header("content-type", "text/event-stream; charset=utf-8")
                .body("data: hello\n\n");
        });
        let client = build_http_client();
        let url = format!("http://127.0.0.1:{}/events", server.port());
        let result = forward_request(
            &client,
            &axum::http::Method::GET,
            &url,
            "127.0.0.1",
            server.port(),
            HeaderMap::new(),
            Bytes::new(),
            2,
        )
        .await
        .unwrap();
        match result {
            ForwardOutcome::Stream(streaming) => {
                assert_eq!(streaming.status, StatusCode::OK);
            }
            ForwardOutcome::Buffered(_) => panic!("event-stream response must not be buffered"),
        }
    }

    #[tokio::test]
    async fn connection_refused_maps_to_link_unavailable() {
        let client = build_http_client();
        let result = forward_request(
            &client,
            &Method::GET,
            "http://127.0.0.1:9/x",
            "127.0.0.1",
            9,
            HeaderMap::new(),
            Bytes::new(),
            2,
        )
        .await;
        assert_eq!(result.unwrap_err(), ForwardError::LinkUnavailable);
    }
}
