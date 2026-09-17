//! The outbound HTTP client of the proxy data plane.
//!
//! [`ProxyClient`] wraps the hyper-util legacy client the plan fixes
//! (`TokioExecutor` + `HttpConnector`), built once per gear instance and shared
//! by every request. It streams in both directions: the request body is
//! forwarded as it is read from the caller (never buffered) and the upstream
//! response body is handed back as an [`axum::body::Body`] the proxy returns
//! untouched, so a `text/event-stream` arrives chunk by chunk.
//!
//! The client is the only place that knows about the transport, so it owns the
//! transport-level failure modes of the proxy pipeline: a call that exceeds
//! `proxy_timeout_secs` (504), a call that never connected or broke mid-flight
//! (502), a body that outgrew the hard limit (413) and a body whose bytes did
//! not add up to its declared `Content-Length` (400). [`ProxyClient::send_upgrade`]
//! is the upgrade path of that contract: it forwards an upgrade handshake, keeps
//! the upstream's `101` and hands the switched socket to
//! [`crate::infra::upgrade::tunnel`], which splices it to the caller's.

use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use futures_util::Stream;
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::domain::error::OagwError;
use crate::domain::proxy::ProxyError;

/// The stream of an inbound [`axum::body::Body`], pinned so it can be polled.
type BodyDataStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

/// The outbound body of a proxied request: the caller's body, streamed, with
/// the size limit and the declared length enforced while the bytes flow.
///
/// `http-body-util` is only a dev-dependency of this crate, so the wrapper is
/// hand-rolled over the `http-body` traits through the `hyper::body`
/// re-exports.
pub struct ProxyRequestBody {
    inner: BodyDataStream,
    /// Bytes handed to the transport so far.
    served: u64,
    /// Length the caller declared, when it declared one: the exact size hint
    /// the transport frames the request with, and the total the body has to
    /// add up to.
    declared: Option<u64>,
    limit: u64,
}

impl std::fmt::Debug for ProxyRequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyRequestBody")
            .field("served", &self.served)
            .field("declared", &self.declared)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl ProxyRequestBody {
    /// Wraps the caller's body.
    ///
    /// `declared` is the `Content-Length` the request carried (`None` for a
    /// chunked or absent one), which becomes the exact size hint of the
    /// outbound body so a mismatch is caught by the transport rather than
    /// silently forwarded.
    #[must_use]
    pub fn new(body: Body, declared: Option<u64>, limit: u64) -> Self {
        Self {
            inner: Box::pin(body.into_data_stream()),
            served: 0,
            declared,
            limit,
        }
    }
}

/// The error of the counting request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProxyBodyError {
    /// The body exceeded [`crate::domain::proxy::MAX_PROXY_BODY_BYTES`].
    #[error("the request body exceeds the proxy limit")]
    TooLarge,
    /// The body carried a `Content-Length` the streamed bytes did not add up to.
    #[error("the request body length differs from its declared Content-Length")]
    LengthMismatch,
}

impl HttpBody for ProxyRequestBody {
    type Data = Bytes;
    type Error = ProxyBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let frame = match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => {
                let declared = self.declared.unwrap_or(self.served);
                if self.served != declared {
                    return Poll::Ready(Some(Err(ProxyBodyError::LengthMismatch)));
                }
                return Poll::Ready(None);
            }
            Poll::Ready(Some(Err(error))) => {
                tracing::debug!(error = %error, "inbound body read failed");
                return Poll::Ready(Some(Err(ProxyBodyError::LengthMismatch)));
            }
            Poll::Ready(Some(Ok(frame))) => frame,
        };
        let length = u64::try_from(frame.len()).unwrap_or(u64::MAX);
        self.served = self.served.saturating_add(length);
        if self.served > self.limit {
            return Poll::Ready(Some(Err(ProxyBodyError::TooLarge)));
        }
        Poll::Ready(Some(Ok(Frame::data(frame))))
    }

    fn size_hint(&self) -> SizeHint {
        match self.declared {
            Some(exact) => SizeHint::with_exact(exact),
            None => SizeHint::default(),
        }
    }
}

impl From<ProxyBodyError> for axum::Error {
    fn from(error: ProxyBodyError) -> Self {
        axum::Error::new(error)
    }
}

/// The routing facts of one proxied call, bundled so the transport signature
/// stays at the three things it actually consumes: the call, the planned
/// headers and the caller's body.
#[derive(Debug, Clone)]
pub struct ProxyCall {
    /// Outbound method.
    pub method: String,
    /// Scheme of the selected endpoint (`http` in this build).
    pub scheme: String,
    /// `host:port` of the selected endpoint, the `Host` the upstream sees.
    pub authority: String,
    /// Path after the alias, already resolved through the matched route.
    pub path: String,
    /// Raw query string of the inbound request, when it carried one.
    pub query: Option<String>,
    /// `Content-Length` the caller declared, which the streamed body has to
    /// add up to.
    pub declared_length: Option<u64>,
    /// Hard limit the streamed request body may not outgrow. The default is
    /// [`crate::domain::proxy::MAX_PROXY_BODY_BYTES`]; the tests lower it.
    pub limit: u64,
    /// Time to the upstream's response headers.
    pub timeout: Duration,
}

impl Default for ProxyCall {
    fn default() -> Self {
        Self {
            method: "GET".to_owned(),
            scheme: "http".to_owned(),
            authority: String::new(),
            path: "/".to_owned(),
            query: None,
            declared_length: None,
            limit: crate::domain::proxy::MAX_PROXY_BODY_BYTES,
            timeout: Duration::from_secs(2),
        }
    }
}

/// The upstream HTTP client of the proxy: a shared hyper-util connection pool.
#[derive(Clone)]
pub struct ProxyClient {
    client: Client<HttpConnector, ProxyRequestBody>,
    round_robin: Arc<AtomicU64>,
}

impl std::fmt::Debug for ProxyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyClient")
            .field("round_robin", &self.round_robin.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for ProxyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyClient {
    /// Builds the shared client.
    ///
    /// No TLS connector is installed: this build proxies `http` endpoints only,
    /// and an `https` endpoint is rejected by the pipeline before the client is
    /// consulted (see the phase notes).
    #[must_use]
    pub fn new() -> Self {
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build_http::<ProxyRequestBody>();
        Self {
            client,
            round_robin: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The round-robin counter of the endpoint pool, incremented per request.
    #[must_use]
    pub fn next_round_robin(&self) -> u64 {
        self.round_robin.fetch_add(1, Ordering::Relaxed)
    }

    /// Dispatches a proxied request to the endpoint `call` names and returns
    /// the streamed upstream response.
    ///
    /// `headers` is the outbound header plan already applied by the api layer,
    /// which sets `Host` from [`ProxyCall::authority`]'s endpoint; the transport
    /// adds nothing. [`ProxyCall::timeout`] covers the time to the upstream's
    /// response headers; the body is streamed afterwards without a cut-off, so a
    /// long-lived `text/event-stream` is not aborted by `proxy_timeout_secs`.
    ///
    /// # Errors
    /// [`OagwError::RequestTimeout`] when the upstream did not answer in time,
    /// [`OagwError::PayloadTooLarge`] when the streamed body outgrew the hard
    /// limit, [`OagwError::Validation`] when the body length differed from its
    /// declared `Content-Length` and
    /// [`ProxyError::DownstreamUnavailable`] when the upstream could not be
    /// reached at all.
    pub async fn send(
        &self,
        call: &ProxyCall,
        headers: http::HeaderMap,
        body: Body,
    ) -> Result<ProxyResponse, ProxyError> {
        let request = self.build_request(
            call,
            headers,
            ProxyRequestBody::new(body, call.declared_length, call.limit),
        )?;
        let response = tokio::time::timeout(call.timeout, self.client.request(request))
            .await
            .map_err(|_| OagwError::RequestTimeout {
                message: format!(
                    "the upstream did not respond within {} ms",
                    call.timeout.as_millis()
                ),
            })?
            .map_err(|error| transport_error(&error))?;
        Ok(ProxyResponse::new(response))
    }

    /// Forwards an upgrade request and returns the upstream's answer.
    ///
    /// The request carries no body, and is framed by its headers alone: with no
    /// declared length the transport writes no `Content-Length`, so the
    /// handshake is the only thing on the wire. [`ProxyCall::timeout`] covers
    /// the time to the handshake's response headers.
    ///
    /// The upstream must answer `101`. Anything else — a refusal, a redirect, a
    /// plain error — is a session the caller asked for that cannot happen, and
    /// is reported as [`ProxyError::DownstreamUnavailable`] rather than relayed:
    /// the caller has no use for an HTTP response to a request that was an
    /// upgrade.
    ///
    /// # Errors
    /// [`OagwError::RequestTimeout`] when the upstream did not answer in time,
    /// [`ProxyError::DownstreamUnavailable`] when it could not be reached or
    /// refused the upgrade and [`OagwError::Validation`] when the outbound
    /// request could not be built.
    pub async fn send_upgrade(
        &self,
        call: &ProxyCall,
        headers: http::HeaderMap,
    ) -> Result<UpstreamUpgrade, ProxyError> {
        let body = ProxyRequestBody::new(Body::empty(), None, call.limit);
        let request = self.build_request(call, headers, body)?;
        let response = tokio::time::timeout(call.timeout, self.client.request(request))
            .await
            .map_err(|_| OagwError::RequestTimeout {
                message: format!(
                    "the upstream did not respond within {} ms",
                    call.timeout.as_millis()
                ),
            })?
            .map_err(|error| transport_error(&error))?;
        let response = ProxyResponse::new(response);
        if !response.is_upgrade() {
            return Err(ProxyError::DownstreamUnavailable {
                message: format!(
                    "the upstream refused the upgrade with status {}",
                    response.status().as_u16()
                ),
            });
        }
        Ok(UpstreamUpgrade::new(response.into_raw()))
    }

    /// Builds the outbound request over the headers the api layer planned.
    fn build_request(
        &self,
        call: &ProxyCall,
        headers: http::HeaderMap,
        body: ProxyRequestBody,
    ) -> Result<http::Request<ProxyRequestBody>, ProxyError> {
        let mut builder = http::Request::builder()
            .method(call.method.as_str())
            .uri(build_uri(
                &call.scheme,
                &call.authority,
                &call.path,
                call.query.as_deref(),
            )?);
        *builder
            .headers_mut()
            .expect("the outbound request headers are always writable") = headers;
        builder.body(body).map_err(|error| {
            OagwError::Validation {
                message: format!("the outbound request could not be built: {error}"),
            }
            .into()
        })
    }
}

/// The absolute URL of the outbound request, query string preserved.
fn build_uri(
    scheme: &str,
    authority: &str,
    path: &str,
    query: Option<&str>,
) -> Result<http::Uri, OagwError> {
    let target = match query {
        Some(query) if !query.is_empty() => format!("{scheme}://{authority}{path}?{query}"),
        _ => format!("{scheme}://{authority}{path}"),
    };
    http::Uri::try_from(target).map_err(|error| OagwError::Validation {
        message: format!("the upstream request URI is not valid: {error}"),
    })
}

/// The counting-body failure the transport wrapped, if it wrapped one.
fn body_error(source: &(dyn StdError + 'static)) -> Option<ProxyBodyError> {
    let mut current: Option<&(dyn StdError + 'static)> = Some(source);
    while let Some(error) = current {
        if let Some(body_error) = error.downcast_ref::<ProxyBodyError>() {
            return Some(*body_error);
        }
        current = error.source();
    }
    None
}

/// Maps a transport failure onto the error the proxy reports: a body that
/// outgrew the limit is a 413, one that mismatched its declared length a 400,
/// and everything else a 502.
fn transport_error(error: &hyper_util::client::legacy::Error) -> ProxyError {
    match body_error(error) {
        Some(ProxyBodyError::TooLarge) => OagwError::PayloadTooLarge {
            message: "the request body exceeds the 100 MiB proxy limit".to_owned(),
        }
        .into(),
        Some(ProxyBodyError::LengthMismatch) => OagwError::Validation {
            message: "the request body length differs from its declared Content-Length".to_owned(),
        }
        .into(),
        None => {
            let message = error
                .source()
                .map_or_else(|| error.to_string(), std::string::ToString::to_string);
            ProxyError::DownstreamUnavailable { message }
        }
    }
}

/// The upstream response, with its body left as a stream.
pub struct ProxyResponse {
    response: http::Response<hyper::body::Incoming>,
}

impl std::fmt::Debug for ProxyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyResponse")
            .field("status", &self.response.status().as_u16())
            .finish_non_exhaustive()
    }
}

impl ProxyResponse {
    /// Wraps the hyper response.
    fn new(response: http::Response<hyper::body::Incoming>) -> Self {
        Self { response }
    }

    /// The status the upstream returned, read before the body is taken.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.response.status()
    }

    /// Whether the upstream accepted an upgrade (the phase-7 tunnel hook).
    #[must_use]
    pub fn is_upgrade(&self) -> bool {
        self.response.status() == http::StatusCode::SWITCHING_PROTOCOLS
    }

    /// Takes the raw hyper response back, upgrade socket and all.
    fn into_raw(self) -> http::Response<hyper::body::Incoming> {
        self.response
    }

    /// Takes the response apart, leaving the body as a stream.
    pub fn into_parts(self) -> (http::StatusCode, http::HeaderMap, Body) {
        let (parts, body) = self.response.into_parts();
        (parts.status, parts.headers, Body::new(body))
    }
}

/// The upstream side of an accepted upgrade: the `101` and the socket it
/// switched to.
///
/// The response is kept whole until [`UpstreamUpgrade::on_upgrade`] is taken:
/// that is where the switched socket lives, and dropping the response before
/// taking it leaves hyper with an upgrade nobody claimed.
pub struct UpstreamUpgrade {
    response: http::Response<hyper::body::Incoming>,
}

impl std::fmt::Debug for UpstreamUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamUpgrade")
            .field("status", &self.response.status().as_u16())
            .finish_non_exhaustive()
    }
}

impl UpstreamUpgrade {
    /// Wraps the hyper response of an accepted upgrade request.
    fn new(response: http::Response<hyper::body::Incoming>) -> Self {
        Self { response }
    }

    /// The status the upgrade got, read before the socket is taken.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.response.status()
    }

    /// The handshake headers the upstream answered with, socket still attached.
    #[must_use]
    pub fn headers(&self) -> &http::HeaderMap {
        self.response.headers()
    }

    /// The upstream end of the switched connection, once the `101` is relayed.
    ///
    /// Must be taken before the response is dropped, which is when hyper
    /// discards an upgrade nobody claimed.
    #[must_use]
    pub fn on_upgrade(&mut self) -> hyper::upgrade::OnUpgrade {
        hyper::upgrade::on(&mut self.response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::time::Duration;

    use std::error::Error as StdError;

    use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST};
    use hyper::body::Body as HttpBody;

    use super::{ProxyBodyError, ProxyClient, build_uri};
    use crate::domain::proxy::ProxyError;

    /// The outbound request of a call, built the way the api layer would.
    fn planned(
        client: &ProxyClient,
        call: &super::ProxyCall,
        headers: http::HeaderMap,
    ) -> http::Request<super::ProxyRequestBody> {
        client
            .build_request(
                call,
                headers,
                super::ProxyRequestBody::new(axum::body::Body::empty(), call.declared_length, 1024),
            )
            .unwrap()
    }

    /// A call to `authority`, with the limit and timeout the tests control.
    fn call(method: &str, authority: &str) -> super::ProxyCall {
        super::ProxyCall {
            method: method.to_owned(),
            authority: authority.to_owned(),
            path: "/v1".to_owned(),
            declared_length: Some(0),
            limit: 1024,
            timeout: Duration::from_millis(500),
            ..super::ProxyCall::default()
        }
    }

    #[test]
    fn the_client_is_a_shared_default() {
        let client = ProxyClient::new();
        assert_eq!(client.next_round_robin(), 0);
        assert_eq!(client.next_round_robin(), 1);
        let cloned = client.clone();
        assert_eq!(cloned.next_round_robin(), 2);
        assert!(format!("{client:?}").contains("ProxyClient"));
    }

    #[test]
    fn the_outbound_uri_keeps_the_query_string() {
        let uri = build_uri("http", "api.openai.com:8080", "/v1/chat", Some("a=1&b=2")).unwrap();
        assert_eq!(
            uri.to_string(),
            "http://api.openai.com:8080/v1/chat?a=1&b=2"
        );

        let uri = build_uri("http", "api.openai.com", "/v1/chat", None).unwrap();
        assert_eq!(uri.to_string(), "http://api.openai.com/v1/chat");

        let uri = build_uri("http", "api.openai.com", "/v1/chat", Some("")).unwrap();
        assert_eq!(uri.to_string(), "http://api.openai.com/v1/chat");

        assert!(build_uri("http", "bad host", "/v1", None).is_err());
    }

    #[test]
    fn the_outbound_request_carries_the_planned_headers() {
        let client = ProxyClient::new();
        let mut headers = http::HeaderMap::new();
        headers.insert(HOST, "api.openai.com".parse().unwrap());
        headers.insert("x-kept", "yes".parse().unwrap());
        headers.insert("x-set", "planned".parse().unwrap());
        let request = planned(&client, &call("POST", "api.openai.com"), headers);
        assert_eq!(request.method(), "POST");
        assert_eq!(request.uri().to_string(), "http://api.openai.com/v1");
        assert_eq!(request.headers()[HOST], "api.openai.com");
        assert_eq!(request.headers()["x-kept"], "yes");
        assert_eq!(request.headers()["x-set"], "planned");
    }

    #[test]
    fn the_transport_leaves_a_missing_host_header_alone() {
        let client = ProxyClient::new();
        let request = planned(
            &client,
            &call("GET", "api.openai.com"),
            http::HeaderMap::new(),
        );
        assert!(request.headers().get(HOST).is_none());
    }

    #[test]
    fn the_transport_carries_a_sized_body_with_its_declared_length() {
        let client = ProxyClient::new();
        let body = super::ProxyRequestBody::new(axum::body::Body::from("payload"), Some(7), 1024);
        let call = super::ProxyCall {
            method: "POST".to_owned(),
            authority: "api.openai.com".to_owned(),
            declared_length: Some(7),
            limit: 1024,
            ..super::ProxyCall::default()
        };
        let request = client
            .build_request(
                &call,
                {
                    let mut headers = http::HeaderMap::new();
                    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
                    headers
                },
                body,
            )
            .unwrap();
        assert_eq!(request.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(request.body().size_hint().exact(), Some(7));
    }

    #[tokio::test]
    async fn a_dead_upstream_is_a_502() {
        let client = ProxyClient::new();
        let call = call("GET", "127.0.0.1:1");
        let error = client
            .send(&call, http::HeaderMap::new(), axum::body::Body::empty())
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 502, "{error:?}");
        assert!(matches!(error, ProxyError::DownstreamUnavailable { .. }));
    }

    #[test]
    fn body_errors_are_recognised_through_the_transport_chain() {
        assert_eq!(
            super::body_error(&ProxyBodyError::TooLarge),
            Some(ProxyBodyError::TooLarge)
        );
        assert_eq!(
            super::body_error(&ProxyBodyError::LengthMismatch),
            Some(ProxyBodyError::LengthMismatch)
        );

        let wrapped = Wrapped(ProxyBodyError::TooLarge);
        assert_eq!(super::body_error(&wrapped), Some(ProxyBodyError::TooLarge));

        assert_eq!(
            super::body_error(&std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused"
            )),
            None
        );
    }

    #[test]
    fn a_declared_length_outside_the_header_is_ignored() {
        // The api layer validates the framing headers; the transport only sees
        // the parsed value. A missing declaration leaves the size hint empty.
        let body = super::ProxyRequestBody::new(axum::body::Body::empty(), None, 1024);
        assert_eq!(body.size_hint().exact(), None);
        let sized = super::ProxyRequestBody::new(axum::body::Body::empty(), Some(12), 1024);
        assert_eq!(sized.size_hint().exact(), Some(12));
        assert_eq!(CONTENT_LENGTH.as_str(), "content-length");
    }

    /// A transport error that carries a body error in its `source` chain.
    #[derive(Debug)]
    struct Wrapped(ProxyBodyError);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapped")
        }
    }

    impl StdError for Wrapped {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.0)
        }
    }

    /// Drives `body` to the end and returns the error it raised, if any.
    fn drained(body: &mut super::ProxyRequestBody) -> Option<ProxyBodyError> {
        use std::task::{Context, Poll};

        let waker = futures_util::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        loop {
            match std::pin::Pin::new(&mut *body).poll_frame(&mut cx) {
                Poll::Ready(Some(Err(error))) => return Some(error),
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(None) => return None,
                Poll::Pending => return None,
            }
        }
    }

    #[test]
    fn a_body_that_outgrows_its_limit_is_rejected_while_streaming() {
        let mut body =
            super::ProxyRequestBody::new(axum::body::Body::from(vec![0_u8; 32]), Some(32), 16);
        assert_eq!(drained(&mut body), Some(ProxyBodyError::TooLarge));
    }

    #[test]
    fn a_body_that_stops_short_of_its_declared_length_is_rejected() {
        let mut body =
            super::ProxyRequestBody::new(axum::body::Body::from("short"), Some(4096), 1024);
        assert_eq!(drained(&mut body), Some(ProxyBodyError::LengthMismatch));
    }

    #[test]
    fn a_body_that_matches_its_declared_length_is_forwarded() {
        let mut body =
            super::ProxyRequestBody::new(axum::body::Body::from("payload"), Some(7), 1024);
        assert_eq!(drained(&mut body), None);
    }
}
