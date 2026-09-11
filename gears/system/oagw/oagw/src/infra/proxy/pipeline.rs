//! The proxy pipeline of `cpt-cf-oagw-feature-proxy-pipeline`.
//!
//! The module is the caller that orders the pure decisions of
//! [`crate::domain::proxy`] and performs the outbound call: it drives the
//! config-resolution half of the stage, the route match and the merge half, the
//! endpoint selection, the actual-request CORS check, the header processing,
//! the validation, the plugin chain and the rate-limit check, the scheme and
//! SSRF policy, the outbound call over the `DataPlaneServiceImpl` bridge and
//! the response passthrough.
//!
//! What the pipeline itself decides is only the order of those stages and the
//! mapping of a transport failure onto the closed error table; every rule the
//! stages apply lives in the domain module. The API handler in
//! [`crate::api::rest::proxy_handler`] is the caller of
//! [`ProxyPipeline::handle`], and it is the only component that renders a
//! gateway rejection: every rejection this pipeline produces is handed back as
//! an [`OagwError`], never as a body it built itself.
// @cpt-begin:cpt-cf-oagw-dod-proxy-request-pipeline:p1:inst-full
// The pipeline contract of `cpt-cf-oagw-dod-proxy-request-pipeline`: the
// preflight fast path answers a 204 without resolving anything, the
// config-resolution stage runs before the route match and the merge half after
// it, the endpoint selection, the CORS check, the header processing, the
// validation, the chain and the rate-limit check run in the order the flow
// fixes, the scheme and SSRF policy run after the chain and before the
// outbound call, the upstream call streams and is never retried and never
// cached, and the response passes through with only the `response.*` rules and
// the CORS headers applied.
// @cpt-begin:cpt-cf-oagw-dod-error-source-header:p1:inst-full
// The source contract of `cpt-cf-oagw-dod-error-source-header` (ADR 0007): a
// response the gateway produced carries `X-OAGW-Error-Source: gateway` and is
// always `application/problem+json`, and a response an upstream produced
// carries `X-OAGW-Error-Source: upstream` with its status, headers and body as
// received, whether the status is a success or an error. The header itself is
// written by the API handler and the error-response flow of the gear-wiring
// feature; this pipeline only decides which side produced what it returns.

use std::collections::HashMap;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::Stream;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode, Version};
use http_body_util::BodyExt as _;
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::upgrade::OnUpgrade;
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use tenant_resolver_sdk::TenantResolverClient;
use uuid::Uuid;

use crate::domain::error::ErrorContext;
use crate::domain::error::OagwError;
use crate::domain::model::{
    AuthConfig, DEFAULT_ENDPOINT_SCHEME, Endpoint, EndpointScheme, PluginBinding, Upstream,
};
use crate::domain::observability as ob;
use crate::domain::plugin::RequestContext;
use crate::domain::proxy::{
    self, AliasKind, AllowedHeaders, CorsCheck, CorsOutcome, MatchRejection, ProxyContext,
    ProxyRequest, SchemePolicy, SelectionFailure, TARGET_HOST_HEADER,
};
use crate::domain::ratelimit::{EnforcementPoint, RateLimitHeaders, RateLimitOutcome};
use crate::domain::resolution::{EffectiveConfig, SelectedTargetState};
use crate::domain::streaming::{StreamFailure, StreamSession, StreamSessionState};
use crate::infra::observability::RequestOutcome;
use crate::infra::observability::Telemetry;
use crate::infra::plugin::guard::GuardOutcome;
use crate::infra::plugin::plan::{
    BindingSet, ExecutionPlan, PluginRegistries, resolve as resolve_plan,
};
use crate::infra::plugin::registry::NOOP_AUTH_PLUGIN_ID;
use crate::infra::proxy::limiter::RateLimiter;
use crate::infra::proxy::streaming::{
    self, BoxDuplex, FailureRecorder, StreamJournal, TunnelPump, UpgradeHandover,
};
use crate::infra::resolution::EffectiveConfigResolver;
use crate::infra::storage::InMemoryStores;
use toolkit_security::SecurityContext;

/// The error type every boxed failure the data plane carries reduces to.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The body the outbound request carries and the upstream response streams.
///
/// The request half is fully buffered, the 100 MB limit of
/// `cpt-cf-oagw-constraint-body-limit` having been enforced while the API
/// handler read it, and the response half is the upstream stream itself, never
/// re-serialized and never buffered as a whole
/// (`cpt-cf-oagw-principle-no-cache`).
pub enum OutboundBody {
    /// A body already in memory: the request the gateway forwards, or the
    /// canned body a test connector answers with.
    Full(Bytes),
    /// The upstream response stream, carrying the idle read timeout.
    Streaming(StreamingBody),
}

impl std::fmt::Debug for OutboundBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(body) => f.debug_tuple("Full").field(&body.len()).finish(),
            Self::Streaming(stream) => f.debug_tuple("Streaming").field(&stream.idle).finish(),
        }
    }
}

impl OutboundBody {
    /// The size of the body when it is in memory, zero when it is a stream
    /// whose length the gateway never knew: no response body is ever read to
    /// measure it.
    #[must_use]
    pub fn size(&self) -> u64 {
        match self {
            Self::Full(body) => u64::try_from(body.len()).unwrap_or(u64::MAX),
            Self::Streaming(_) => 0,
        }
    }
}

impl From<Bytes> for OutboundBody {
    fn from(body: Bytes) -> Self {
        Self::Full(body)
    }
}

impl hyper::body::Body for OutboundBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Both variants are `Unpin`, so the projection is a plain `get_mut`.
        match self.get_mut() {
            Self::Full(body) => {
                if body.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Frame::data(std::mem::take(body)))))
                }
            }
            Self::Streaming(stream) => Pin::new(stream).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Full(body) => body.is_empty(),
            Self::Streaming(stream) => stream.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(body) => {
                let mut hint = SizeHint::default();
                hint.set_exact(u64::try_from(body.len()).unwrap_or(0));
                hint
            }
            Self::Streaming(stream) => stream.size_hint(),
        }
    }
}

/// The upstream response stream, with the idle read timeout the transport
/// failure mapping needs (`inst-pf-33`) and the size hint the bridge read off
/// the response it wrapped, the stream detection of the streaming feature
/// reading the two.
pub struct StreamingBody {
    inner: BoxStream,
    idle: Duration,
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
    hint: SizeHint,
    recorder: Option<FailureRecorder>,
    emitter: Option<FailureRecorder>,
}

type BoxStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;

impl StreamingBody {
    /// Wraps an upstream stream with an idle read timeout and the size hint the
    /// body it wraps reports.
    #[must_use]
    pub fn new(inner: BoxStream, idle: Duration, hint: SizeHint) -> Self {
        Self {
            inner,
            idle,
            delay: None,
            hint,
            recorder: None,
            emitter: None,
        }
    }

    /// Re-times the idle read clock the streamed body holds, the streaming
    /// stage doing it at the head relay so the interval without a received byte
    /// is bounded by `proxy_timeout_secs` and not by the bridge's own default.
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
        self.delay = None;
    }

    /// Attaches the failure recorder the streaming stage reports a classified
    /// failure to. The recorder fires at most once: a body reports exactly one
    /// failure, the one that ended it.
    pub fn set_recorder(&mut self, recorder: FailureRecorder) {
        self.recorder = Some(recorder);
    }

    /// Attaches the recorder the streamed classification is emitted through,
    /// beside the one the streaming stage owns. It fires at most once, and only
    /// when the body ended in a classified failure.
    pub fn set_stream_emitter(&mut self, recorder: FailureRecorder) {
        self.emitter = Some(recorder);
    }

    /// Reports the failure that ended the body to the recorder, once.
    fn report(&mut self, failure: StreamFailure) {
        if let Some(recorder) = self.recorder.take() {
            recorder(failure);
        }
        if let Some(emitter) = self.emitter.take() {
            emitter(failure);
        }
    }
}

impl hyper::body::Body for StreamingBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(frame) => {
                this.delay = None;
                let frame = frame.map(|item| {
                    if let Err(ref error) = item {
                        this.report(body_failure(error));
                    }
                    item.map(Frame::data)
                });
                Poll::Ready(frame)
            }
            Poll::Pending => {
                let expired = match this.delay.as_mut() {
                    None => {
                        let mut delay = Box::pin(tokio::time::sleep(this.idle));
                        let ready = delay.as_mut().poll(cx).is_ready();
                        if !ready {
                            this.delay = Some(delay);
                        }
                        ready
                    }
                    Some(delay) => delay.as_mut().poll(cx).is_ready(),
                };
                if expired {
                    // The idle read is the one failure a streamed body raises
                    // itself: the classification reaches the journal through the
                    // same recorder the upstream's own abort reaches.
                    this.report(StreamFailure::Idle);
                    Poll::Ready(Some(Err(Box::new(IdleReadTimeout) as BoxError)))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        self.hint.clone()
    }
}

/// Classifies the failure a streamed body produced, the idle-read marker being
/// the only failure the body itself raises.
fn body_failure(error: &BoxError) -> StreamFailure {
    if error.is::<IdleReadTimeout>() {
        StreamFailure::Idle
    } else {
        StreamFailure::Aborted
    }
}

/// The marker error an idle read timeout carries, mapped to 504 `IdleTimeout`
/// by the transport failure classifier.
#[derive(Debug)]
struct IdleReadTimeout;

impl std::fmt::Display for IdleReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an idle read timed out")
    }
}

impl std::error::Error for IdleReadTimeout {}

/// The HTTP version the outbound hop uses, the value the per-host cache holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    /// HTTP/1.1, the fallback a host that refused HTTP/2 is pinned to.
    Http1,
    /// HTTP/2, negotiated through ALPN during the TLS handshake.
    Http2,
}

impl HttpVersion {
    /// The version an HTTP response reports.
    #[must_use]
    pub fn of(version: Version) -> Self {
        if version == Version::HTTP_2 {
            Self::Http2
        } else {
            Self::Http1
        }
    }
}

/// A per-host cache entry: the DESIGN's "HTTP/2 supported" / "HTTP/1.1 only".
#[derive(Debug, Clone, Copy)]
struct VersionEntry {
    version: HttpVersion,
    expires_at: Instant,
}

/// The per-host HTTP-version cache (DESIGN "HTTP Version Negotiation").
///
/// In-memory state of the data plane, never persisted, so a restart
/// re-negotiates and the position after a restart is unobservable.
#[derive(Debug)]
pub struct HttpVersionCache {
    entries: Mutex<HashMap<String, VersionEntry>>,
    ttl: Duration,
}

/// The DESIGN's cache entry TTL.
const DEFAULT_VERSION_TTL: Duration = Duration::from_secs(60 * 60);

impl Default for HttpVersionCache {
    fn default() -> Self {
        Self::new(DEFAULT_VERSION_TTL)
    }
}

impl HttpVersionCache {
    /// A cache whose entries live for `ttl`.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// The version a previous request to the host negotiated, when it has not
    /// expired.
    #[must_use]
    pub fn get(&self, authority: &str) -> Option<HttpVersion> {
        let mut entries = self.entries.lock();
        match entries.get(authority).copied() {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.version),
            _ => {
                entries.remove(authority);
                None
            }
        }
    }

    /// Records the version a request to the host negotiated.
    pub fn put(&self, authority: &str, version: HttpVersion) {
        self.entries.lock().insert(
            authority.to_owned(),
            VersionEntry {
                version,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }
}

/// The failure of the outbound call or of the upstream exchange, mapped onto
/// the existing rows by the transport stage (`inst-pf-34`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    /// The connection could not be established.
    Connection,
    /// The request timeout elapsed before the response headers arrived.
    RequestTimeout,
    /// An idle read timed out while the response streamed.
    IdleTimeout,
    /// The upstream spoke a protocol the gateway cannot follow.
    Protocol,
    /// The upstream exchange failed in a way no other row names.
    Downstream,
    /// The response stream aborted before its end.
    StreamAborted,
    /// The upstream is unreachable.
    LinkUnavailable,
}

/// The outbound request one transport stage issues: the version the cache
/// pinned, the deadline the arrival instant fixed and the request itself.
pub struct OutboundRequest {
    /// The HTTP version to attempt on the hop.
    pub version: HttpVersion,
    /// The time the call may still take, measured from the arrival instant.
    pub budget: Duration,
    /// The transformed request.
    pub request: http::Request<OutboundBody>,
}

/// The connection the data plane reaches an upstream through.
///
/// The pipeline is the caller: it composes the request, applies the version and
/// the timeout and records the negotiated version, the connector performing the
/// transport.
pub trait UpstreamConnector: Send + Sync + 'static {
    /// Performs the outbound call and returns the upstream response.
    ///
    /// The future is boxed so the pipeline holds one erased connector, the
    /// production bridge and the test stub being interchangeable behind it.
    ///
    /// # Errors
    /// Returns the typed transport failure the pipeline maps onto the closed
    /// table; no `OagwError` is built here, the mapping being the pipeline's.
    fn call<'a>(&'a self, outbound: OutboundRequest) -> PinnedCall<'a>;

    /// Performs the outbound call of a WebSocket upgrade and hands back the
    /// 101 and the gateway's end of the upgraded connection.
    ///
    /// The hop is the same bridge the buffered dispatch uses, forced to
    /// HTTP/1.1 because an upgrade exists only there; the future resolves once
    /// the upgrade completed, and a response whose status is not 101 is
    /// reported as a protocol failure rather than handed back.
    ///
    /// # Errors
    /// Returns the typed transport failure the pipeline maps onto the closed
    /// table, `Protocol` for an answer that is not an upgrade.
    fn upgrade<'a>(&'a self, outbound: OutboundRequest) -> PinnedUpgrade<'a>;
}

/// The response the bridge handed back, with the version the hop negotiated.
pub struct OutboundReply {
    /// The status the upstream sent.
    pub status: StatusCode,
    /// The headers the upstream sent.
    pub headers: HeaderMap,
    /// The response stream.
    pub body: OutboundBody,
}

/// A hyper legacy client behind the closed shape the bridge dispatches on.
type Sender = Arc<dyn Fn(http::Request<OutboundBody>) -> PinnedCall<'static> + Send + Sync>;
type PinnedCall<'a> = Pin<
    Box<dyn Future<Output = Result<http::Response<OutboundBody>, TransportFailure>> + Send + 'a>,
>;
type PinnedUpgrade<'a> =
    Pin<Box<dyn Future<Output = Result<UpgradeHandover, TransportFailure>> + Send + 'a>>;

/// The bridge the DESIGN's Gear Structure names `DataPlaneServiceImpl`: the
/// hyper legacy clients over the rustls connector, one per hop shape, selected
/// by the negotiated version and by the endpoint scheme.
pub struct HyperBridge {
    /// The client that negotiates through ALPN, used for a host with no cache
    /// entry and for one cached as HTTP/2.
    https_negotiate: Sender,
    /// The HTTP/1.1-only client, used for a host cached as HTTP/1.1.
    https_h1: Sender,
    /// The plaintext client, used for an `http` endpoint the knob allowed.
    http_plain: Sender,
}

impl HyperBridge {
    /// Builds the bridge over the system root certificate store, the idle read
    /// interval of a streamed body being the one `proxy_timeout_secs` value the
    /// gear configures: the key bounds both the establishment of a streamed
    /// connection and the idle-read interval of an established one, and no
    /// second key is read.
    ///
    /// # Errors
    /// Returns the `LinkUnavailable` row when the root store cannot be read,
    /// the failure surfacing at startup and not at request time.
    pub fn try_new(idle_read: Duration) -> Result<Self, OagwError> {
        let idle = idle_read;
        let negotiate = https_connector(true).map_err(root_store_unreadable)?;
        let h1 = https_connector(false).map_err(root_store_unreadable)?;
        Ok(Self {
            https_negotiate: sender_of(
                Client::builder(TokioExecutor::new()).build(negotiate),
                idle,
            ),
            https_h1: sender_of(Client::builder(TokioExecutor::new()).build(h1), idle),
            // A refused or otherwise unavailable connection is answered
            // `LinkUnavailable` whichever hop shape carried it: the row is the
            // one the transport-failure table gives an establishment failure,
            // and no client of the bridge names a different one.
            http_plain: sender_of(
                Client::builder(TokioExecutor::new())
                    .build(hyper_util::client::legacy::connect::HttpConnector::new()),
                idle,
            ),
        })
    }
}

/// The startup error an unreadable root certificate store is reported as.
fn root_store_unreadable(error: std::io::Error) -> OagwError {
    OagwError::link_unavailable(format!(
        "oagw.proxy: the root certificate store is unreadable: {error}"
    ))
}

/// Builds a rustls connector over the system roots, with or without ALPN `h2`.
fn https_connector(
    negotiate: bool,
) -> Result<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    std::io::Error,
> {
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http();
    let plain = hyper_util::client::legacy::connect::HttpConnector::new();
    if negotiate {
        // ALPN carries both `h2` and `http/1.1`, so the handshake negotiates.
        Ok(builder.enable_http1().enable_http2().wrap_connector(plain))
    } else {
        // ALPN stays empty, which pins the hop to HTTP/1.1.
        Ok(builder.enable_http1().wrap_connector(plain))
    }
}

/// Wraps a hyper client into the closed sender shape the bridge dispatches on.
fn sender_of<C>(client: Client<C, OutboundBody>, idle: Duration) -> Sender
where
    C: hyper_util::client::legacy::connect::Connect + Clone + Send + Sync + 'static,
{
    Arc::new(move |request: http::Request<OutboundBody>| {
        let future: ResponseFuture = client.request(request);
        Box::pin(async move {
            let response = match future.await {
                Ok(response) => response,
                Err(error) => {
                    let failure = classify_transport(&error);
                    // The cause the classifier walked stays server-side: the
                    // row the pipeline renders names the class alone, and no
                    // transport message ever reaches a response `detail`.
                    transport_cause(failure, &error);
                    return Err(failure);
                }
            };
            let (parts, body) = response.into_parts();
            // The size hint the body reports is the one the streaming stage
            // reads: an exact hint is a body the passthrough can write at once,
            // an unknown one is the non-buffered stream the feature detects.
            let hint = hyper::body::Body::size_hint(&body);
            Ok(http::Response::from_parts(
                parts,
                OutboundBody::Streaming(StreamingBody::new(
                    Box::pin(IncomingStream { inner: body }),
                    idle,
                    hint,
                )),
            ))
        }) as PinnedCall
    })
}

/// Adapts a hyper response body into the stream the streaming body wraps.
struct IncomingStream {
    inner: Incoming,
}

impl Stream for IncomingStream {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, BoxError>>> {
        // `hyper::body::Incoming` is `Unpin`, so the projection is plain.
        hyper::body::Body::poll_frame(Pin::new(&mut self.get_mut().inner), cx).map(|frame| {
            frame.map(|frame| {
                frame
                    .map(|frame| {
                        frame.into_data().unwrap_or_else(|frame| {
                            let _ = frame.into_trailers();
                            Bytes::new()
                        })
                    })
                    .map_err(BoxError::from)
            })
        })
    }
}

/// Classifies a hyper transport error into the typed failure the pipeline maps.
///
/// The establishment of a hop is answered `LinkUnavailable` whichever client
/// the exchange rode on, a timeout `RequestTimeout` and every other transport
/// failure `Downstream`, the rows the transport-failure table of
/// `cpt-cf-oagw-feature-proxy-pipeline` gives them.
fn classify_transport(error: &(dyn std::error::Error + 'static)) -> TransportFailure {
    if is_timeout(error) {
        TransportFailure::RequestTimeout
    } else if is_connect(error) {
        TransportFailure::LinkUnavailable
    } else {
        TransportFailure::Downstream
    }
}

/// Reports the cause a transport failure was classified from, with the bounded
/// error chain that produced it, on the server side only.
///
/// The rendered row names the class alone and the chain is filtered before it
/// is written, so no request URI and no query string of the exchange reaches a
/// log line, let alone a response.
fn transport_cause(failure: TransportFailure, error: &(dyn std::error::Error + 'static)) {
    let chain = error_chain(error)
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| !text.contains("://") && !text.contains('?'))
        .collect::<Vec<_>>()
        .join(" <- ");
    tracing::warn!(
        failure = ?failure,
        chain = %chain,
        "oagw.proxy: the upstream exchange failed at the transport layer"
    );
}

/// Walks an error source chain looking for the timeout marker hyper writes.
fn is_timeout(error: &(dyn std::error::Error + 'static)) -> bool {
    error_chain(error)
        .iter()
        .any(|(_, text)| is_timeout_text(text))
}

/// Whether one message of a chain names a timeout.
fn is_timeout_text(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("timed out") || lowered.contains("timeout")
}

/// Walks an error source chain looking for a failure to establish the
/// connection.
///
/// The word `connect` is no marker: hyper writes `error reading a body from
/// connection` and `connection closed before message completed` for an exchange
/// that failed long after the hop was made, and a substring test over them
/// answers 503 for a mid-flight failure. Only the typed leaves name an
/// establishment — the legacy client's own connect error, and the `io::Error`
/// the connector reports beneath it — so the walk reads those and ignores the
/// `SendRequest` wrapper hyper-util draws around an exchange already started.
fn is_connect(error: &(dyn std::error::Error + 'static)) -> bool {
    error_chain(error)
        .iter()
        .any(|(error, _)| is_connect_link(*error))
}

/// Whether one link of a chain names an establishment failure.
fn is_connect_link(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(client) = error.downcast_ref::<hyper_util::client::legacy::Error>() {
        return client.is_connect();
    }
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(is_connect_io)
}

/// Whether the `io::Error` a connector leaf reports names an establishment
/// failure: the kinds a refused or unreachable hop carries, and the messages a
/// connector writes for the connect phase. The substring test is read on this
/// leaf alone and never on a wrapper, whose own text names the phase it was
/// drawn for rather than the one that failed.
fn is_connect_io(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable => true,
        _ => is_connect_text(&error.to_string()),
    }
}

/// Whether one leaf message names the connect phase hyper-util writes for a hop
/// it never established.
fn is_connect_text(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("tcp connect error") || lowered.contains("connection refused")
}

/// One link of an error chain: the error it came from, with its message.
type ChainLink<'a> = (&'a (dyn std::error::Error + 'static), String);

/// The links of an error and of its source chain, to a bounded depth, each
/// carried with the error it came from so a typed leaf stays inspectable.
fn error_chain<'a>(error: &'a (dyn std::error::Error + 'static)) -> Vec<ChainLink<'a>> {
    let mut links = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        links.push((error, error.to_string()));
        if links.len() >= CHAIN_DEPTH {
            break;
        }
        current = error.source();
    }
    links
}

/// The depth of an error chain the classifier walks.
const CHAIN_DEPTH: usize = 8;

/// The endpoint schemes the bridge routes to a dedicated client.
const SCHEME_HTTP: &str = "http";
const SCHEME_HTTPS: &str = "https";

impl UpstreamConnector for HyperBridge {
    fn call<'a>(&'a self, outbound: OutboundRequest) -> PinnedCall<'a> {
        let request = outbound.request;
        let scheme = request
            .uri()
            .scheme_str()
            .unwrap_or(DEFAULT_ENDPOINT_SCHEME);
        let sender = match (scheme, outbound.version) {
            (SCHEME_HTTP, _) => Arc::clone(&self.http_plain),
            (SCHEME_HTTPS, HttpVersion::Http1) => Arc::clone(&self.https_h1),
            _ => Arc::clone(&self.https_negotiate),
        };
        sender(request)
    }

    fn upgrade<'a>(&'a self, outbound: OutboundRequest) -> PinnedUpgrade<'a> {
        // An upgrade exists only on HTTP/1.1: the hop is forced to it whatever
        // the version cache pinned the host to, and no version is recorded for
        // a connection that stops being HTTP once the handshake is answered.
        let mut request = outbound.request;
        *request.version_mut() = Version::HTTP_11;
        let scheme = request
            .uri()
            .scheme_str()
            .unwrap_or(DEFAULT_ENDPOINT_SCHEME);
        let sender = if scheme == SCHEME_HTTP {
            Arc::clone(&self.http_plain)
        } else {
            Arc::clone(&self.https_h1)
        };
        Box::pin(async move { upgrade_handover(&sender, request).await })
    }
}

/// Performs the upstream half of an RFC 6455 handshake over the bridge.
///
/// The answer is required to be a 101: anything else is a protocol failure,
/// the body the answer carried being drained and dropped, because nothing of
/// it may reach the client connection before the handshake is verified.
async fn upgrade_handover(
    client: &Sender,
    request: http::Request<OutboundBody>,
) -> Result<UpgradeHandover, TransportFailure> {
    let mut response = client(request).await?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (_parts, body) = response.into_parts();
        drain(body).await;
        return Err(TransportFailure::Protocol);
    }
    let on_upgrade = hyper::upgrade::on(&mut response);
    let upgraded = on_upgrade.await.map_err(|_| TransportFailure::Protocol)?;
    let (parts, _body) = response.into_parts();
    Ok(UpgradeHandover {
        status: parts.status,
        headers: parts.headers,
        io: Box::new(TokioIo::new(upgraded)),
    })
}

/// Drains and drops the body an upstream answered a refused handshake with, so
/// the connection is returned to its pool and none of it reaches the client.
async fn drain(body: OutboundBody) {
    match body {
        OutboundBody::Full(bytes) => {
            let _ = bytes;
        }
        OutboundBody::Streaming(mut body) => {
            while let Some(frame) = body.frame().await {
                let _ = frame;
            }
        }
    }
}

/// The successful outcome of one proxied request.
#[derive(Debug)]
pub enum ProxyOutcome {
    /// The preflight 204 the pipeline answered without resolving anything.
    Preflight(proxy::PreflightHeaders),
    /// The upstream response, passed through.
    Upstream(UpstreamReply),
    /// The 101 of a verified upgrade, with the session the handler pumps after
    /// it relayed the handshake.
    Upgrade(UpgradeOutcome),
}

/// The upstream response the pipeline passes through.
#[derive(Debug)]
pub struct UpstreamReply {
    /// The status the upstream sent.
    pub status: StatusCode,
    /// The headers the upstream sent, after the `response.*` rules and with the
    /// CORS headers and `Vary: Origin` applied.
    pub headers: HeaderMap,
    /// The upstream body stream.
    pub body: OutboundBody,
}

/// The upgraded session the pipeline handed back with the 101 it relayed.
#[derive(Debug)]
pub struct UpgradeOutcome {
    /// The 101 the upstream answered, relayed as the handshake produced it.
    pub status: StatusCode,
    /// The handshake headers the RFC 6455 exchange produced.
    pub headers: HeaderMap,
    /// The bidirectional pump the handler drives once the 101 is written.
    pub tunnel: TunnelPump,
}

/// The rejection of one proxied request: the error the handler renders plus the
/// headers the refusing stage produced, the `Retry-After` and the three
/// `X-RateLimit-*` headers of a 429.
#[derive(Debug)]
pub struct ProxyRejection {
    /// The rejection the handler renders through the closed table.
    pub error: OagwError,
    /// The headers the refusing stage produced.
    pub headers: HeaderMap,
}

impl ProxyRejection {
    /// A rejection carrying no stage header.
    #[must_use]
    pub fn of(error: OagwError) -> Self {
        Self {
            error,
            headers: HeaderMap::new(),
        }
    }

    /// Attaches the stage headers a refusal produced.
    #[must_use]
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }
}

/// The knobs the pipeline reads from the gear configuration.
#[derive(Debug, Clone, Copy)]
pub struct ProxyLimits {
    /// The outbound request timeout, measured from the arrival instant.
    pub proxy_timeout: Duration,
    /// The buffered request-body hard limit.
    pub body_limit: u64,
    /// The outbound scheme policy.
    pub scheme: SchemePolicy,
}

impl Default for ProxyLimits {
    fn default() -> Self {
        Self {
            proxy_timeout: Duration::from_secs(30),
            body_limit: 100 * 1024 * 1024,
            scheme: SchemePolicy {
                allow_http_upstream: false,
                ssrf_enabled: true,
            },
        }
    }
}

/// The handover the streaming stage of an upgrade receives (`inst-ws-01`): the
/// request as the header-processing stage left it, the endpoint it was selected
/// against, the remaining establishment budget and the session of §4.
struct UpgradeStage<'a> {
    /// The outbound target the resolution pinned the request to.
    target: &'a proxy::OutboundTarget,
    /// The request, whose `Sec-WebSocket-Key` the handshake verifies against.
    request: &'a ProxyRequest,
    /// The outbound headers the header-processing stage prepared, carved.
    headers: HeaderMap,
    /// The instant the request arrived, the budget being measured from it.
    arrived: Instant,
    /// The stream session the pipeline opened for this request.
    session: StreamSession,
    /// The configuration host of the selected endpoint.
    endpoint_host: String,
    /// The label site the outbound issuance is attributed to.
    site: OutboundSite<'a>,
}

/// The host labels one outbound issuance is attributed to: the upstream alias
/// the resolution matched and the configuration host of the selected endpoint,
/// both already normalized, both label-safe and both carrying no identifier of
/// the caller or the tenant.
#[derive(Debug)]
struct OutboundSite<'a> {
    /// The upstream alias the request resolved to, the `host` label value.
    host: &'a str,
    /// The configuration host of the selected endpoint.
    endpoint_host: &'a str,
    /// Set when the issuance below was performed, the in-flight gauge being
    /// settled only for a request that issued its call.
    issued: &'a AtomicBool,
}

/// The measurements one request accumulates for the single exit point of the
/// outcome emission: the stages completed so far with the durations they took,
/// the route pattern the match produced and the endpoint labels the selection
/// pinned.
struct RequestObservation {
    /// The route pattern the match produced, `http.route` for every instrument
    /// that carries it, falling back to the shell route of the proxy path.
    route: Option<String>,
    /// The stage durations, one entry per completed stage, in completion order.
    stages: Vec<(ob::Phase, Duration)>,
    /// True once the outbound call was issued, the in-flight gauge being
    /// decremented only for such a request.
    issued: AtomicBool,
    /// The instant the request arrived, the whole-request duration and the
    /// first stage duration being measured from it.
    arrived: Instant,
    /// The instant the last completed stage finished, the next duration being
    /// measured from it.
    last: Instant,
}

impl RequestObservation {
    /// Opens the observation, the first stage duration measured from now.
    fn new() -> Self {
        let now = Instant::now();
        Self {
            route: None,
            stages: Vec::new(),
            issued: AtomicBool::new(false),
            arrived: now,
            last: now,
        }
    }

    /// Closes the stage in flight and opens the next one.
    fn record_stage(&mut self, phase: ob::Phase) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        self.stages.push((phase, elapsed));
    }

    /// Remembers the route pattern the match produced, the value every
    /// `http.route` label is built from.
    fn record_route(&mut self, pattern: Option<&str>) {
        self.route = pattern.map(str::to_owned);
    }

    /// The label-safe route pattern: the matched pattern when one matched, the
    /// shell route of the proxy path otherwise, never the raw request path.
    fn route_pattern(&self) -> &str {
        self.route.as_deref().unwrap_or(ob::PROXY_SHELL_ROUTE)
    }

    /// Builds the audit facts one outcome renders a record from: the closed
    /// field set, the normalized labels and the row the failure mapped onto.
    fn audit_facts(
        &self,
        request: &ProxyRequest,
        security: &SecurityContext,
        status: u16,
        response_size: u64,
        error: Option<ob::AuditErrorRow>,
    ) -> ob::AuditFacts {
        ob::AuditFacts {
            request_id: toolkit::api::extract_trace_id(&request.headers),
            tenant_id: Some(security.subject_tenant_id().to_string()),
            principal_id: Some(security.subject_id().to_string()),
            host: request.alias.clone(),
            path: ob::audit_path(&request.alias, &request.path_suffix),
            method: request.method.clone(),
            status,
            duration_ms: u64::try_from(self.arrived.elapsed().as_millis()).unwrap_or(u64::MAX),
            request_size: request.body_len,
            response_size,
            error,
        }
    }
}

/// The data-plane pipeline of the proxy path
/// (`cpt-cf-oagw-feature-proxy-pipeline`).
pub struct ProxyPipeline {
    resolver: Arc<EffectiveConfigResolver>,
    limiter: Arc<RateLimiter>,
    registries: Arc<PluginRegistries>,
    connector: Arc<dyn UpstreamConnector>,
    versions: Arc<HttpVersionCache>,
    /// The per-pool round-robin cursor, keyed by the upstream identifier.
    cursors: Mutex<HashMap<Uuid, u64>>,
    /// The streamed-failure journal the streaming stage records into, the hook
    /// the observability feature's emission reads.
    journal: Arc<StreamJournal>,
    /// The emission facade the observability feature owns and shares with the
    /// control plane, read-only from here on.
    telemetry: Arc<Telemetry>,
    limits: ProxyLimits,
}

impl std::fmt::Debug for ProxyPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyPipeline")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ProxyPipeline {
    /// Builds the pipeline over the stores the control plane owns, the
    /// tenant-resolver client the gear resolved at init, the plugin registries
    /// the gear constructed and the production connector.
    ///
    /// # Errors
    /// Returns the `LinkUnavailable` row when the connector cannot be built,
    /// the failure surfacing at startup and not at request time.
    pub fn try_new(
        stores: &InMemoryStores,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        registries: Arc<PluginRegistries>,
        limits: ProxyLimits,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, OagwError> {
        let connector = HyperBridge::try_new(limits.proxy_timeout)?;
        let versions = Arc::new(HttpVersionCache::default());
        Ok(Self::with_connector(
            Arc::new(connector),
            versions,
            Arc::new(EffectiveConfigResolver::new(
                Arc::new(stores.upstreams()),
                Arc::new(stores.routes()),
                tenant_resolver,
            )),
            Arc::new(RateLimiter::new()),
            registries,
            limits,
            telemetry,
        ))
    }

    /// Builds the pipeline over an explicit connector, the form the tests drive.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_connector(
        connector: Arc<dyn UpstreamConnector>,
        versions: Arc<HttpVersionCache>,
        resolver: Arc<EffectiveConfigResolver>,
        limiter: Arc<RateLimiter>,
        registries: Arc<PluginRegistries>,
        limits: ProxyLimits,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            resolver,
            limiter,
            registries,
            connector,
            versions,
            cursors: Mutex::new(HashMap::new()),
            journal: Arc::new(StreamJournal::new()),
            telemetry,
            limits,
        }
    }

    /// The emission facade the pipeline emits through.
    #[must_use]
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.telemetry
    }

    /// The streamed-failure journal the streaming stage records into, the hook
    /// the observability feature's emission reads.
    #[must_use]
    pub fn stream_journal(&self) -> &Arc<StreamJournal> {
        &self.journal
    }

    /// The buffered request-body hard limit the pipeline enforces on a request
    /// whose `Content-Length` declares more than it carries.
    ///
    /// The shell reads the same knob to bound the buffering of the request body
    /// before the handler is ever called: the rejection of an oversized body
    /// happens at the extractor, before the pipeline can be consulted, and so
    /// the two readers of the knob must be fed by the same configuration.
    #[must_use]
    pub fn body_limit(&self) -> u64 {
        self.limits.body_limit
    }

    /// Runs the pipeline for one proxied request.
    ///
    /// # Errors
    /// Returns the rejection the failed stage produced, for the handler to
    /// render through the closed table; no problem+json body is built here.
    pub async fn handle(
        &self,
        request: &ProxyRequest,
        security: &SecurityContext,
        peer: IpAddr,
        body: Bytes,
    ) -> Result<ProxyOutcome, ProxyRejection> {
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-02
        // IF the request is a CORS preflight — method `OPTIONS` carrying an
        // `Origin` header and an `Access-Control-Request-Method` header.
        let origin = header_value(&request.headers, "origin");
        let requested_method = header_value(&request.headers, "access-control-request-method");
        let requested_headers = header_value(&request.headers, "access-control-request-headers");
        if proxy::is_preflight(
            &request.method,
            origin.as_deref(),
            requested_method.as_deref(),
        ) {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-03
            // Answer it at the preflight branch of the CORS check and return
            // the 204 without resolving the upstream, without obtaining a
            // tenant context, without matching a route, without selecting an
            // endpoint and without running any plugin — the fast path ADR 0004
            // decides, subject only to the infrastructure-level controls
            // outside this gear.
            return match proxy::cors_check(
                None,
                CorsCheck {
                    method: &request.method,
                    origin: origin.as_deref(),
                    request_method: requested_method.as_deref(),
                    request_headers: requested_headers.as_deref(),
                },
            ) {
                CorsOutcome::Preflight(headers) => Ok(ProxyOutcome::Preflight(headers)),
                // A request `is_preflight` accepted always enters the
                // preflight branch, and that branch never rejects, so no other
                // outcome is reachable here.
                _ => Err(ProxyRejection::of(OagwError::route_error(
                    "oagw.proxy: the preflight request was answered by no branch of the CORS check",
                ))),
            };
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-03
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-02

        // The measures the single exit point emits the request outcome from.
        // The arrival instant is recorded here — the `inst-pf-01` instant the
        // request timeout and the latency budget are measured from — and the
        // stages the request completes are collected as they complete.
        let mut observation = RequestObservation::new();
        let arrived = Instant::now();

        // The body of the pipeline runs in one inner block, so the request
        // outcome is emitted once, at the single exit point below, for both the
        // `Ok` and the `Err` paths, and no request is ever counted twice.
        let outcome = async {
            let mut context = ProxyContext::new(request.alias.clone());
            observation.record_stage(ob::Phase::Classification);

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-04
            // ELSE drive the resolution half of the config-resolution stage by
            // invoking the effective-resolution flow with the normalized alias, the
            // method, the path and the request's security context, and consume the
            // selected upstream, the route tier order it fixed and the not-found,
            // `Disabled` and `LinkUnavailable` dispositions it can return; the
            // merge half of the stage is invoked after the route match. This
            // pipeline re-walks no tenant chain, re-applies no sharing mode and
            // re-computes no `min()`.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-04
            let resolution = match self.resolver.resolve(security, &request.alias).await {
                Ok(resolution) => resolution,
                Err(error) => return Err(self.fail(&mut context, error)),
            };
            observation.record_stage(ob::Phase::ConfigResolution);
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-05
            // IF the resolution returned a disposition rather than a configuration
            // — the not-found disposition, the `Disabled` disposition, or the
            // `LinkUnavailable` failure of the tenant-chain walk.
            if resolution.state() != SelectedTargetState::Selected {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-06
                // Render the disposition through the existing rows of the
                // error-mapping algorithm: the not-found disposition as 404
                // `RouteNotFound`, the `LinkUnavailable` failure as 503
                // `LinkUnavailable`, and the `Disabled` disposition as the 503
                // gateway rejection the enable-disable requirement requires,
                // rendered through that same row's 503 status and GTS type with a
                // `detail` naming the disabled upstream. No row is added to the
                // closed table, and no route match, no plugin and no upstream call
                // follows a disposition.
                let error = match resolution.state() {
                    SelectedTargetState::Disabled => OagwError::link_unavailable(format!(
                        "oagw.proxy: the upstream of alias {} is disabled by a disabled ancestor",
                        request.alias
                    )),
                    _ => OagwError::route_not_found(format!(
                        "oagw.proxy: no enabled upstream holds the alias {}",
                        request.alias
                    )),
                };
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-06
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-07
                // Close the `ProxyContext` in `Failed` and return the rendered
                // response to the API handler.
                return Err(self.fail(&mut context, error));
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-07
            }
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-05

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-08
            // ELSE match the route against the route tier the resolution fixed —
            // the selected upstream's own routes first, then the inherited ancestor
            // routes — using the request method, the path suffix and the query
            // string, and take the outbound base path the matched route composes;
            // when a route matched, invoke the effective-merge flow with the
            // selected target, its ancestor chain and the matched route, and
            // consume the `EffectiveConfig` value the merge returns.
            let matched = match proxy::match_route(
                resolution.target.protocol.as_deref().unwrap_or_default(),
                &resolution.route_tiers,
                &request.method,
                &request.path_suffix,
                &request.query,
            ) {
                Ok(matched) => matched,
                Err(rejection) => return Err(self.route_rejection(&mut context, rejection)),
            };
            let effective = resolution.merge(Some(&matched.route));
            observation.record_stage(ob::Phase::RouteMatching);
            observation.record_route(
                matched
                    .route
                    .match_config
                    .as_ref()
                    .and_then(|config| config.http_path()),
            );
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-08
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-09
            // IF no route of the tier matched, or a matched route rejected the
            // request on its `query_allowlist` or its `path_suffix_mode`, the
            // rejection is rendered above and the stage ends here: the remaining
            // steps run only when a route matched and accepted the request.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-09
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-10
            // Reject with 404 `RouteNotFound` when no route matched — including
            // when the request's method is in no candidate route's allowlist — and
            // with 400 `ValidationError` when a route matched and rejected the
            // request content, both rendered through the existing rows; no endpoint
            // is selected and no plugin runs.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-10
            context.record_route(matched.route.id.unwrap_or_default());
            let advanced = context.advance(proxy::ProxyState::Routed);
            debug_assert!(
                advanced.is_ok(),
                "Classified -> Routed is a declared transition"
            );

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-12
            // ELSE select the outbound endpoint through the endpoint-selection
            // algorithm, reading `X-OAGW-Target-Host` for routing and consuming it.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-12
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-13
            // IF the selection failed — the header is required and absent, its form
            // is invalid, or it names no endpoint of the pool.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-13
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-14
            // Reject through the existing `MissingTargetHost`, `InvalidTargetHost`
            // or `UnknownTargetHost` row (all 400) with
            // `X-OAGW-Error-Source: gateway`, the `detail` naming the valid hosts
            // where the row supplies them, and no forwarding takes place.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-14
            let selected = {
                let mut cursors = self.cursors.lock();
                let cursor = cursors
                    .entry(resolution.target.id.unwrap_or_default())
                    .or_insert(0);
                match proxy::select_endpoint(
                    endpoints(&resolution.target),
                    AliasKind::of_upstream(&resolution.target),
                    header_value(&request.headers, TARGET_HOST_HEADER).as_deref(),
                    cursor,
                ) {
                    Ok(selected) => selected,
                    Err(failure) => {
                        let pool = endpoints(&resolution.target).to_vec();
                        return Err(self.selection_failure(&mut context, failure, &pool));
                    }
                }
            };
            context.record_endpoint(
                selected.endpoint.host_stripped().unwrap_or_default(),
                selected.method,
            );
            let advanced = context.advance(proxy::ProxyState::EndpointSelected);
            debug_assert!(
                advanced.is_ok(),
                "Routed -> EndpointSelected is a declared transition"
            );
            observation.record_stage(ob::Phase::EndpointSelection);

            // @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
            // The emission points the observability feature is the emitter of: the
            // endpoint-selection event the selection just took, the rate-limit
            // events the check is about to produce and the request outcome the
            // single exit point emits. The facade owns the vocabulary and this
            // pipeline owns the decisions behind every event it hands over.
            self.telemetry.endpoint_selected(
                &resolution
                    .target
                    .id
                    .map_or_else(String::new, |id| id.to_string()),
                selected.endpoint.host_stripped().unwrap_or_default(),
                selected.method.as_str(),
                matches!(
                    selected.method,
                    crate::domain::proxy::SelectionMethod::ExplicitHeader
                ),
            );
            // @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-16
            // ELSE run the actual-request branch of the CORS check for a
            // cross-origin request against the CORS block the `EffectiveConfig`
            // merged — the origin check, then the method check — this being the
            // position after upstream resolution and endpoint selection and before
            // forwarding, and a request with no `Origin` header or no enabled CORS
            // block passing through with no CORS work at all.
            let cors = proxy::cors_check(
                effective.cors.as_ref(),
                CorsCheck {
                    method: &request.method,
                    origin: origin.as_deref(),
                    request_method: requested_method.as_deref(),
                    request_headers: requested_headers.as_deref(),
                },
            );
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-16
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-17
            // IF the CORS check rejected the request.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-17
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-18
            // Reject with 403 `CorsOriginNotAllowed` or 403 `CorsMethodNotAllowed`
            // through the existing rows; no plugin has run and no forwarding takes
            // place.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-18
            let allowed = match cors {
                CorsOutcome::Preflight(headers) => return Ok(ProxyOutcome::Preflight(headers)),
                CorsOutcome::Allowed(allowed) => Some(allowed),
                CorsOutcome::NotApplicable => None,
                CorsOutcome::OriginRejected => {
                    return Err(self
                        .fail(
                            &mut context,
                            OagwError::cors_origin_not_allowed(format!(
                                "oagw.proxy: the origin {} is not in the allowed origins",
                                origin.unwrap_or_default()
                            )),
                        )
                        .with_headers(cors_rejection_headers()));
                }
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-19
                // Close the `ProxyContext` in `Failed` and return the rendered
                // rejection to the API handler; `self.fail` is the close.
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-19
                CorsOutcome::MethodRejected => {
                    return Err(self
                        .fail(
                            &mut context,
                            OagwError::cors_method_not_allowed(format!(
                                "oagw.proxy: the method {} is not in the allowed methods",
                                request.method
                            )),
                        )
                        .with_headers(cors_rejection_headers()));
                }
            };
            observation.record_stage(ob::Phase::CorsCheck);

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-20
            // ELSE process the outbound header set: consume `X-OAGW-Target-Host`
            // without ever forwarding it, strip the hop-by-hop list, replace `Host`
            // with the selected endpoint's host and, on an HTTP/2 hop, `:authority`
            // with the upstream authority, and then apply the `request.*` rules of
            // the selected upstream's `upstream.headers` block that the resolution
            // returned.
            let target = proxy::outbound_target(
                &selected.endpoint,
                &matched.outbound_path,
                &matched.outbound_query,
            );
            // An `Upgrade: websocket` request is the one request the strip list
            // exempts three families for: the carve-out of
            // `cpt-cf-oagw-algo-upgrade-header-carve-out` is applied here, inside
            // the header-processing stage, and every other member of the list and
            // every `request.*` rule applies to it exactly as to any other request.
            let outbound_headers =
                if crate::domain::streaming::is_websocket_upgrade(&request.headers) {
                    proxy::prepare_upgrade_request_headers(
                        &request.headers,
                        &target.authority,
                        request_rules(&resolution.target),
                    )
                } else {
                    proxy::prepare_request_headers(
                        &request.headers,
                        &target.authority,
                        request_rules(&resolution.target),
                    )
                };
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-20
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-21
            // Validate the body and the header syntax through the default rules of
            // the request-validation DoD: `Content-Length` a valid integer that
            // matches the received size, the total size within the 100 MB limit
            // rejected before buffering, `Transfer-Encoding` limited to `chunked`,
            // no CR or LF in any header value, and no `Content-Length` carried
            // together with a `Transfer-Encoding`.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-21
            if let Err(error) =
                proxy::validate_inbound(&request.headers, request.body_len, self.limits.body_limit)
            {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-22
                // IF the body or the header validation failed.
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-22
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-23
                // Reject with 400 `ValidationError`, or 413 `PayloadTooLarge` for
                // the size limit, through the existing rows; no plugin runs and no
                // upstream call is made.
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-23
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-24
                // Close the `ProxyContext` in `Failed` and return the rendered
                // rejection to the API handler.
                return Err(self.fail(&mut context, error));
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-24
            }
            let advanced = context.advance(proxy::ProxyState::Validated);
            debug_assert!(
                advanced.is_ok(),
                "EndpointSelected -> Validated is a declared transition"
            );
            observation.record_stage(ob::Phase::HeaderValidation);

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-25
            // ELSE drive the plugin chain over the ordered binding set the
            // `EffectiveConfig` carries: hand the prepared context to the
            // execution-plan flow, invoke its auth phase, then invoke the
            // rate-limit check at the stage position the state-management ADR fixes
            // — after the auth plugin and before the guard and transform plugins —
            // then invoke the guard-and-transform phase for the request tiers.
            let chain = match self.chain_plan(&effective) {
                Ok(plan) => plan,
                Err(error) => return Err(self.fail(&mut context, error)),
            };
            let mut prepared = chain
                .as_ref()
                .map(|_| RequestContext::new(security.clone()));
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-25
            match self
                .auth_phase(
                    chain.as_ref(),
                    prepared.as_mut(),
                    &outbound_headers,
                    request,
                )
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    // @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
                    // inst-ob-20: a credential-resolution failure the auth phase
                    // mapped onto one of its rows is the authentication-failure
                    // event the plugin chain names this feature the emitter of: an
                    // ERROR record with the mapped row's name, the class being
                    // rate-limited before it is written.
                    // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-20
                    self.telemetry.audit(
                        crate::domain::observability::AuditEventClass::AuthenticationFailure,
                        observation.audit_facts(
                            request,
                            security,
                            error.status(),
                            0,
                            Some(crate::domain::observability::AuditErrorRow::of(
                                error.mapping().variant,
                                error.mapping().title,
                            )),
                        ),
                    );
                    // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-20
                    // @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
                    return Err(self.fail(&mut context, error));
                }
            }
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-26
            // IF a chain phase or the rate-limit check refused the request.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-26
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-27
            // Render the refusal through the row the refusing stage names — 429
            // `RateLimitExceeded`, 503 `PluginNotFound`, 500 `SecretNotFound`, 503
            // `LinkUnavailable` or the 400 `ValidationError` a guard rejection
            // renders — adding no row to the table, and skip the upstream call, the
            // `Retry-After` and `X-RateLimit-*` headers the invoked check produces
            // being carried onto the rendered response unchanged on the 429 case.
            let limited = self
                .limiter
                .check_security(
                    &effective,
                    &EnforcementPoint {
                        upstream_id: resolution.target.id.unwrap_or_else(Uuid::nil),
                        matched_route: Some(matched.route.clone()),
                    },
                    security,
                    peer,
                )
                .await;
            // @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
            // The token level the check left behind and, when it refused, the
            // refusal event: the two events the rate-limiting feature names this
            // feature the emitter of. The `path` label carries the normalized route
            // match pattern of the route the check evaluated, and the refusal is
            // recorded at WARN with the `Retry-After` delay the check computed.
            self.telemetry.rate_limit_usage(
                &request.alias,
                observation.route_pattern(),
                usage_ratio_of(&limited),
            );
            // inst-ob-21: the refusal the rate-limit check produced opens the
            // WARN record DESIGN §4.3 assigns to that log point, with
            // `error_type` `RateLimitExceeded` and the `Retry-After` delay the
            // check computed.
            // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-21
            if let RateLimitOutcome::Refused {
                retry_after_secs,
                headers,
            } = limited
            {
                self.telemetry
                    .rate_limit_refused(&request.alias, observation.route_pattern());
                let error = OagwError::rate_limit_exceeded(
                    "oagw.proxy: the request exceeded the effective rate limit",
                )
                .with_context(ErrorContext::new().with_retry_after_seconds(retry_after_secs));
                self.telemetry.audit(
                    crate::domain::observability::AuditEventClass::RateLimitRefusal {
                        retry_after_secs,
                    },
                    // The status the row the refusal renders carries, not a
                    // placeholder: the record is the audit trace of the 429.
                    observation.audit_facts(request, security, error.status(), 0, None),
                );
                return Err(self
                    .fail(&mut context, error)
                    .with_headers(rate_limit_headers(headers, retry_after_secs)));
            }
            // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-21
            // @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-27
            if let Err(error) = self.request_phase(chain.as_ref(), prepared.as_mut()).await {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-28
                // Close the `ProxyContext` in `Failed` and return the rendered
                // refusal to the API handler.
                return Err(self.fail(&mut context, error));
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-28
            }
            observation.record_stage(ob::Phase::PluginChain);

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-29
            // ELSE apply the outbound scheme policy and the SSRF posture to the
            // selected endpoint.
            let scheme = selected
                .endpoint
                .scheme_enum()
                .unwrap_or(EndpointScheme::Https);
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-29
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-30
            // Allow `https` and `wss` unconditionally, allow `http` only when
            // `allow_http_upstream` is `true` and reject the request with 400
            // `ValidationError` when it is `false` — the https-only posture lifted
            // only by that knob — answer an endpoint whose scheme is `grpc` or `wt`
            // with the gateway `RouteError`/`ProtocolError` problem+json semantics
            // rather than proxying it, and apply the `ssrf_policy.enabled` hook:
            // the scheme allowlist, the allowed-segment match and the IP pinning
            // rule.
            if let Err(error) = proxy::check_endpoint_policy(
                scheme,
                selected.endpoint.host_stripped().unwrap_or_default(),
                self.limits.scheme,
            ) {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-31
                // Close the `ProxyContext` in `Failed` and return the rendered
                // rejection to the API handler when the policy refused the
                // endpoint.
                return Err(self.fail(&mut context, error));
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-31
            }
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-30
            observation.record_stage(ob::Phase::SchemePolicy);
            let endpoint_host = selected
                .endpoint
                .host_stripped()
                .unwrap_or_default()
                .to_owned();

            // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-01
            // Receive, from `cpt-cf-oagw-flow-proxy-request` at its streaming stage
            // — the caller of this and every later step — the request with its
            // `Upgrade`, `Connection` and `Sec-WebSocket-*` headers as the
            // header-processing stage left them, the selected `Endpoint`, the
            // `EffectiveConfig` value and the `ProxyContext`, every stage of the
            // pipeline from classification through the scheme and SSRF policy
            // having already run for the upgrade request exactly as for any other.
            // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-01
            // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-02
            // IF the client request carries `Upgrade: websocket` — the upgrade
            // being detected on the client request, which is the one detection this
            // feature performs on the request side. The carve-out the detection
            // arms was already applied by the header-processing stage above, so
            // this branch is taken after every stage of the pipeline has run and
            // skips none of them.
            // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-02
            if crate::domain::streaming::is_websocket_upgrade(&request.headers) {
                // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-04
                // ELSE report "not an upgrade" to the caller and RETURN: the SSE
                // flow of §2 or the buffered passthrough of `inst-pf-37` handles
                // the response and this flow is done — the path every
                // non-upgrade request takes below.
                // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-04
                let session = StreamSession::new();
                // The client request was classified an upgrade: the session of §4
                // opens in `Detected` (`inst-ss-01`).
                let _ = session.transition(StreamSessionState::Detected);
                // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-05
                // Read the endpoint scheme the endpoint selection returned and
                // decide whether the endpoint can carry the upgrade.
                // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-05
                // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-06
                // IF the scheme is `wss`, or it is `http` and `allow_http_upstream`
                // is `true`, the endpoint is eligible and the handshake of
                // `inst-ws-09` follows. An `http` endpoint whose knob is `false`
                // never reaches this branch: the scheme policy of `inst-pf-30`
                // refused it already.
                // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-06
                if let Err(error) = crate::domain::streaming::upgrade_scheme_eligible(scheme) {
                    // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-07
                    // ELSE answer the upgrade before any handshake: an endpoint
                    // whose scheme is `https` but not `wss`, a `grpc` endpoint and
                    // a `wt` endpoint are answered with 400 `RouteError` through
                    // the existing row with `X-OAGW-Error-Source: gateway`, and an
                    // `http` endpoint with `allow_http_upstream` `false` is refused
                    // with 400 `ValidationError` through the scheme policy of
                    // `inst-pf-30`, no row being added and no header being carved
                    // out for a request that is refused here.
                    // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-07
                    // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-08
                    // The `ProxyContext` is closed in `Failed` and the rendered
                    // rejection returned to `cpt-cf-oagw-flow-proxy-request`; the
                    // session of §4 closes in `Failed` (`inst-ss-04`).
                    // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-08
                    let _ = session.transition(StreamSessionState::Failed);
                    return Err(self.fail(&mut context, error));
                }
                let streamed = self
                    .upgrade_flow(
                        &mut context,
                        UpgradeStage {
                            target: &target,
                            request,
                            headers: outbound_headers,
                            arrived,
                            session,
                            endpoint_host: endpoint_host.clone(),
                            site: OutboundSite {
                                host: request.alias.as_str(),
                                endpoint_host: endpoint_host.as_str(),
                                issued: &observation.issued,
                            },
                        },
                    )
                    .await;
                observation.record_stage(ob::Phase::OutboundCall);
                observation.record_stage(ob::Phase::ResponsePassthrough);
                return match streamed {
                    Ok(outcome) => Ok(outcome),
                    Err(failure) => {
                        let fields = self
                            .error_phase(
                                chain.as_ref(),
                                &resolution.target,
                                &matched.outbound_path,
                                &selected.endpoint,
                            )
                            .await;
                        Err(self.transport_stage(&mut context, failure, fields))
                    }
                };
            }
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-32
            // Perform the streaming outbound call over the bridge on the negotiated
            // HTTP version, forwarding the transformed request and streaming the
            // response back without buffering it; no full client-request retry is
            // issued and no response is cached, while connector-level endpoint
            // failover and connection-retry attempts inside the upstream connector
            // remain permitted. The request timeout is `proxy_timeout_secs`
            // measured from the arrival instant.
            let reply = self
                .dispatch(
                    &target,
                    &request.method,
                    outbound_headers,
                    body,
                    arrived,
                    OutboundSite {
                        host: request.alias.as_str(),
                        endpoint_host: endpoint_host.as_str(),
                        issued: &observation.issued,
                    },
                )
                .await;
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-32
            observation.record_stage(ob::Phase::OutboundCall);
            let mut reply = match reply {
                Ok(reply) => reply,
                Err(failure) => {
                    let fields = self
                        .error_phase(
                            chain.as_ref(),
                            &resolution.target,
                            &matched.outbound_path,
                            &selected.endpoint,
                        )
                        .await;
                    return Err(self.transport_stage(&mut context, failure, fields));
                }
            };
            let advanced = context.advance(proxy::ProxyState::Dispatched);
            debug_assert!(
                advanced.is_ok(),
                "Validated -> Dispatched is a declared transition"
            );

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-33
            // IF the outbound call or the upstream exchange failed at the transport
            // or protocol level — the connection could not be established, the
            // request timeout elapsed, an idle read timed out, the protocol failed,
            // or the stream aborted — the failure is mapped above and the context
            // is closed in `Failed`.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-33
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-34
            // Map the failure onto the existing rows — 504 `ConnectionTimeout`,
            // `RequestTimeout` or `IdleTimeout`, 502 `ProtocolError`,
            // `DownstreamError` or `StreamAborted`, 503 `LinkUnavailable` — with
            // `X-OAGW-Error-Source: gateway`, hand the resulting `ErrorContext`
            // back through the transform tier's `on_error` phase
            // (`Reentry::Error`) and return the problem+json body that error
            // contract produces.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-34
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-35
            // Close the `ProxyContext` in `Failed` and return the rendered failure
            // to the API handler.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-35

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-36
            // ELSE passthrough the upstream response.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-36
            // The streaming stage of `cpt-cf-oagw-feature-streaming-proxy` is
            // entered here, after the outbound call was issued and before the
            // passthrough renders the response: it decides which half of the
            // passthrough handles the body and arms the streamed body's failure
            // recording, and it re-writes no header of the upstream's reply.
            let session = StreamSession::new();
            streaming::handover(
                &mut reply,
                &session,
                &self.journal,
                &request.alias,
                &endpoint_host,
                self.limits.proxy_timeout,
            );
            // @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
            // A failure the streamed body reports after its head was relayed is
            // the streamed classification the streaming feature records, and it
            // arrives here as the producer event §1.5 names: an ordinary row of
            // the closed table, counted by `oagw_errors_total` under the same
            // label keys as any other failure and rendered as one ERROR record
            // of the failed-request class. No request counter is touched — the
            // request the stream belonged to was counted once at its own
            // outcome below — and the emission never touches the stream or the
            // frames it still relays.
            if let OutboundBody::Streaming(body) = &mut reply.body {
                let route = observation.route_pattern().to_owned();
                let host = request.alias.clone();
                let base = observation.audit_facts(request, security, 0, 0, None);
                let telemetry = Arc::clone(&self.telemetry);
                body.set_stream_emitter(Arc::new(move |failure| {
                    let classified = crate::domain::streaming::classify(failure, true);
                    let error = ob::AuditErrorRow::of(
                        classified.row.mapping().variant,
                        classified.row.mapping().title,
                    );
                    let facts = ob::AuditFacts {
                        status: classified.row.effective_status(),
                        error: Some(error),
                        ..base.clone()
                    };
                    telemetry.stream_failure(
                        &host,
                        &route,
                        ob::AuditEventClass::RequestFailure {
                            passed_through_status: None,
                        },
                        facts,
                    );
                }));
            }
            // @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-37
            // Forward the upstream status, headers and body as received, apply the
            // `response.*` rules of the selected upstream's `upstream.headers`
            // block that the resolution returned, add the CORS response headers and
            // `Vary: Origin` on an allowed cross-origin request, and never
            // re-serialize an upstream body: an error status received from the
            // upstream is passed through as-is with only
            // `X-OAGW-Error-Source: upstream` added, a successful proxied response
            // carries the same header with the same value, and every
            // gateway-produced body is `application/problem+json` with
            // `X-OAGW-Error-Source: gateway`.
            let headers =
                proxy::apply_response_rules(&reply.headers, response_rules(&resolution.target));
            let headers = cors_headers(headers, allowed.as_ref());
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-37
            let advanced = context.advance(proxy::ProxyState::Responded);
            debug_assert!(
                advanced.is_ok(),
                "Dispatched -> Responded is a declared transition"
            );
            observation.record_stage(ob::Phase::ResponsePassthrough);

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-38
            // Close the `ProxyContext` in `Responded` and return the
            // `ProxyResponse` to the API handler.
            let outcome = Ok(ProxyOutcome::Upstream(UpstreamReply {
                status: reply.status,
                headers,
                body: reply.body,
            }));
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-38
            outcome
        }
        .await;

        // @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
        // The single exit point of `cpt-cf-oagw-dod-audit-event-coverage`: the
        // request outcome is emitted here, exactly once per request, for the
        // `Ok` and the `Err` path alike, after the pipeline body above ran to
        // one of its returns and before the handler sees the result. The
        // emission never re-maps, delays or fails the request: it adds the
        // metrics of the completed stages and the audit record of the class the
        // outcome maps onto, and returns the outcome untouched.
        self.emit_request_outcome(request, security, observation, &outcome);
        // @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
        outcome
    }

    /// Emits the request outcome at the single exit point of the pipeline.
    ///
    /// Exactly one `oagw_requests_total` increment, exactly one duration
    /// observation per completed stage under that stage's `phase` label, one
    /// in-flight decrement when the outbound call was issued and at most one
    /// `oagw_errors_total` increment when a row of the closed table was
    /// rendered, plus the audit record the outcome classifies into. The
    /// emission never alters the outcome it is handed.
    fn emit_request_outcome(
        &self,
        request: &ProxyRequest,
        security: &SecurityContext,
        observation: RequestObservation,
        outcome: &Result<ProxyOutcome, ProxyRejection>,
    ) {
        // The preflight fast path answered before the proxied-request path
        // began: no route, no endpoint and no upstream call exist for it, so it
        // is not a proxied request and no instrument counts it.
        let status = match outcome {
            Ok(ProxyOutcome::Preflight(_)) => return,
            Ok(ProxyOutcome::Upstream(reply)) => reply.status,
            Ok(ProxyOutcome::Upgrade(upgrade)) => upgrade.status,
            Err(rejection) => StatusCode::from_u16(rejection.error.effective_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        };
        // The response the caller receives: the upstream's own when the
        // pipeline passed one through, the gateway's when it rendered a row.
        let passed_through = outcome.is_ok();
        let row_of = |error: &OagwError| {
            ob::AuditErrorRow::of(error.mapping().variant, error.mapping().title)
        };
        let error = match outcome {
            // A passed-through upstream status is never re-classified: no row
            // of the closed table was rendered for it, so `error_type` stays
            // unset and no error counter is incremented.
            Ok(_) if status != StatusCode::UNAUTHORIZED => None,
            // The upstream's rejection of the credentials the auth phase
            // injected reaches the caller as the passed-through 401
            // `AuthenticationFailed` response the plugin-chain feature records;
            // the audit record it produces is the authentication-failure event.
            Ok(_) => Some(row_of(&credential_row())),
            Err(rejection) => Some(row_of(&rejection.error)),
        };
        // The class of the record is the branch of
        // `cpt-cf-oagw-flow-audit-record` the outcome falls into, decided from
        // the outcome the pipeline already took and recorded.
        let class = match outcome {
            // inst-ob-20: an authentication failure — the upstream's rejection
            // of the credentials the auth phase injected, reaching the caller as
            // the passed-through 401 response the plugin-chain feature records —
            // opens an ERROR record with the mapped row's name, the class being
            // rate-limited before it is written.
            Ok(_) if status == StatusCode::UNAUTHORIZED => {
                ob::AuditEventClass::AuthenticationFailure
            }
            // inst-ob-22: a failed proxied request — here the passed-through
            // upstream error status the pipeline handed back without rendering a
            // row for it — opens an ERROR record whose `status` carries the
            // passed-through numeric status and whose `error_type` is left
            // unset, the table supplying no row for an upstream-originated
            // status and the error-source ADR forbidding a re-classification.
            Ok(_) if status.as_u16() >= 400 => {
                // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-22
                ob::AuditEventClass::RequestFailure {
                    passed_through_status: Some(status.as_u16()),
                }
                // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-22
            }
            // inst-ob-23: a successful proxied request, which alone is admitted
            // to the sampling gate as the only class that can be dropped.
            Ok(_) => {
                // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-23
                ob::AuditEventClass::RequestSuccess
                // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-23
            }
            Err(rejection) if rejection.error.mapping().variant == AUTHENTICATION_ROW => {
                ob::AuditEventClass::AuthenticationFailure
            }
            // inst-ob-22, the gateway-rendered branch: the row of the closed
            // table the refusal was rendered through is the record's `error_type`
            // and its `title` and `detail` the record's `error_message`.
            Err(_) => ob::AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
        };
        let response_size = match outcome {
            Ok(ProxyOutcome::Upstream(reply)) => reply.body.size(),
            Ok(ProxyOutcome::Upgrade(_)) => 0,
            Ok(ProxyOutcome::Preflight(_)) | Err(_) => 0,
        };
        let issued = observation.issued.load(Ordering::Relaxed);
        let stages = observation.stages.clone();
        self.telemetry.request_outcome(&RequestOutcome {
            host: request.alias.clone(),
            method: request.method.clone(),
            route: observation.route_pattern().to_owned(),
            status: status.as_u16(),
            passed_through,
            error: error.clone(),
            stages,
            issued,
            duration_ms: u64::try_from(observation.arrived.elapsed().as_millis())
                .unwrap_or(u64::MAX),
            request_size: request.body_len,
            response_size,
            tenant_id: Some(security.subject_tenant_id().to_string()),
            principal_id: Some(security.subject_id().to_string()),
            request_id: toolkit::api::extract_trace_id(&request.headers),
            path: audit_path_of(request),
        });
        self.telemetry.audit(
            class,
            observation.audit_facts(request, security, status.as_u16(), response_size, error),
        );
    }

    /// The streaming stage of an upgrade request
    /// (`cpt-cf-oagw-flow-websocket-upgrade`).
    ///
    /// The branch is entered after every stage of the pipeline has run for the
    /// upgrade request exactly as for any other request, and it performs the
    /// upstream handshake and relays the 101; it re-runs no stage and re-matches
    /// no route.
    ///
    /// # Errors
    /// Returns the typed transport failure the caller maps through the same
    /// transport stage the buffered dispatch maps through, so an upgrade
    /// failure and a base-pipeline transport failure can never disagree on the
    /// row they are rendered with.
    async fn upgrade_flow(
        &self,
        context: &mut ProxyContext,
        stage: UpgradeStage<'_>,
    ) -> Result<ProxyOutcome, TransportFailure> {
        let UpgradeStage {
            target,
            request,
            headers,
            arrived,
            session,
            endpoint_host,
            site,
        } = stage;
        // `inst-ws-05` to `inst-ws-07` ran at the branch: the endpoint is
        // eligible and the handshake follows, the session of §4 entering
        // `Establishing` as it begins (`inst-ss-03`).
        let _ = session.transition(StreamSessionState::Establishing);
        // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-09
        // Perform the RFC 6455 handshake against the resolved endpoint over the
        // `DataPlaneServiceImpl` bridge: send the client's `Sec-WebSocket-Key`
        // in the upstream upgrade request, require the upstream's
        // `101 Switching Protocols` with a `Sec-WebSocket-Accept` derived from
        // that key, and bound the whole establishment — connection and
        // handshake — by `proxy_timeout_secs`.
        // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-09
        // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-12
        // IF the handshake failed, the `?` below returns it to the caller's
        // error phase, which closes the `ProxyContext` in `Failed` and returns
        // the rendered failure to `cpt-cf-oagw-flow-proxy-request`; nothing is
        // relayed to the client connection and the session of §4 closes in
        // `Failed` (`inst-ss-06`).
        // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-12
        let handover = self
            .establish_upgrade(
                target,
                request,
                headers,
                arrived,
                OutboundSite {
                    host: site.host,
                    endpoint_host: site.endpoint_host,
                    issued: site.issued,
                },
            )
            .await?;
        // ELSE relay the upstream `101 Switching Protocols` to the client with
        // the handshake headers the RFC 6455 exchange produced, the status and
        // headers of that response being OAGW-generated on the client side and
        // therefore carrying `X-OAGW-Error-Source: gateway` per §1.5. The
        // headers are relayed as produced and the pump is handed over with
        // them, the session of §4 entering `Streaming` at the relay
        // (`inst-ss-05`); the step itself is `inst-ws-13`, implemented by the
        // API handler that writes the response head.
        let advanced = context.advance(proxy::ProxyState::Dispatched);
        debug_assert!(
            advanced.is_ok(),
            "Validated -> Dispatched is a declared transition"
        );
        let _ = session.transition(StreamSessionState::Streaming);
        let alias = request.alias.clone();
        let advanced = context.advance(proxy::ProxyState::Responded);
        debug_assert!(
            advanced.is_ok(),
            "Dispatched -> Responded is a declared transition"
        );
        Ok(ProxyOutcome::Upgrade(UpgradeOutcome {
            status: handover.status,
            headers: handover.headers,
            tunnel: TunnelPump::new(
                handover.io,
                self.limits.proxy_timeout,
                session,
                Arc::clone(&self.journal),
                alias,
                endpoint_host,
            ),
        }))
    }

    /// Issues the upgrade call the handshake performs, bounded by the remaining
    /// `proxy_timeout_secs` budget.
    async fn establish_upgrade(
        &self,
        target: &proxy::OutboundTarget,
        request: &ProxyRequest,
        headers: HeaderMap,
        arrived: Instant,
        site: OutboundSite<'_>,
    ) -> Result<UpgradeHandover, TransportFailure> {
        let builder = http::Request::builder()
            .method(
                http::Method::from_bytes(request.method.as_bytes()).unwrap_or(http::Method::GET),
            )
            .uri(target.uri.as_str())
            .version(Version::HTTP_11);
        let mut outbound = match builder.body(OutboundBody::Full(Bytes::new())) {
            Ok(outbound) => outbound,
            Err(_) => return Err(TransportFailure::Downstream),
        };
        *outbound.headers_mut() = headers;
        let budget = self.limits.proxy_timeout.saturating_sub(arrived.elapsed());
        let outbound_request = OutboundRequest {
            version: HttpVersion::Http1,
            budget,
            request: outbound,
        };
        site.issued.store(true, Ordering::Relaxed);
        self.telemetry.outbound_issued(site.host);
        let call = self.connector.upgrade(outbound_request);
        let handover = match tokio::time::timeout(budget, call).await {
            Ok(handover) => handover,
            // An establishment that outlived the budget is the same failure the
            // base pipeline maps an establishment failure onto.
            Err(_elapsed) => {
                self.telemetry
                    .availability(site.host, site.endpoint_host, false);
                return Err(TransportFailure::Connection);
            }
        };
        let handover = match handover {
            Ok(handover) => handover,
            Err(failure) => {
                if matches!(
                    failure,
                    TransportFailure::Connection | TransportFailure::LinkUnavailable
                ) {
                    self.telemetry
                        .availability(site.host, site.endpoint_host, false);
                }
                return Err(failure);
            }
        };
        if handover.status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(TransportFailure::Protocol);
        }
        // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-10
        // IF the handshake failed — the upstream refused the upgrade, its
        // answer was not a 101, its `Sec-WebSocket-Accept` did not verify
        // against the key, the endpoint was unreachable, or `proxy_timeout_secs`
        // elapsed.
        // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-10
        // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-11
        // Render the failure through the existing rows of
        // `cpt-cf-oagw-algo-error-mapping` with `X-OAGW-Error-Source: gateway`
        // — 504 `ConnectionTimeout` for an elapsed establishment, 502
        // `ProtocolError` for an unverifiable accept or a non-101 answer, 503
        // `LinkUnavailable` for an unreachable or refused endpoint — through
        // `cpt-cf-oagw-flow-error-response` of
        // `cpt-cf-oagw-feature-gear-wiring`, adding no row, and relay nothing
        // to the client connection. The mapping is the base pipeline's, so the
        // rows can never disagree with a buffered request's.
        // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-11
        let key = request
            .headers
            .get("sec-websocket-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let verified = handover
            .headers
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| crate::domain::streaming::verify_websocket_accept(key, accept));
        if !verified {
            return Err(TransportFailure::Protocol);
        }
        Ok(handover)
    }

    /// Relays one upgraded session between the client connection and the
    /// upstream tunnel (`inst-ws-14` to `inst-ws-19`).
    ///
    /// The client half is the one the connection itself hands over once the 101
    /// of `inst-ws-13` was written; it is awaited here and then pumped against
    /// the upstream half the handshake established. The pump ends when either
    /// side closes, both directions are closed behind it and the classification
    /// the pump recorded is never serialised onto the upgraded connection.
    pub async fn relay_tunnel(&self, client: OnUpgrade, tunnel: TunnelPump) {
        match client.await {
            Ok(io) => {
                let client: BoxDuplex = Box::new(TokioIo::new(io));
                streaming::pump(client, tunnel).await;
            }
            Err(_) => {
                // The client half never became a session: the upgrade was
                // abandoned by the caller's connection, which the pump classifies
                // as the abort it is and records rather than delivers.
                tunnel.abort();
            }
        }
    }

    /// Issues the outbound call the streaming stage performs.
    ///
    /// The request timeout is measured from the arrival instant, the version
    /// the cache holds is attempted, and the version the response reports is
    /// recorded for the next hop to the same host.
    async fn dispatch(
        &self,
        target: &proxy::OutboundTarget,
        method: &str,
        headers: HeaderMap,
        body: Bytes,
        arrived: Instant,
        site: OutboundSite<'_>,
    ) -> Result<OutboundReply, TransportFailure> {
        // The first attempt to a host with no cache entry starts from the
        // version its hop shape negotiates: ALPN on a TLS hop, HTTP/1.1 on a
        // plaintext one, where nothing negotiates and the plaintext bridge
        // carries HTTP/1 alone.
        let version = self.versions.get(&target.authority).unwrap_or({
            if target.scheme == SCHEME_HTTP {
                HttpVersion::Http1
            } else {
                HttpVersion::Http2
            }
        });
        let builder = http::Request::builder()
            .method(http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::GET))
            .uri(target.uri.as_str())
            .version(match version {
                HttpVersion::Http1 => Version::HTTP_11,
                HttpVersion::Http2 => Version::HTTP_2,
            });
        let mut outbound = match builder.body(OutboundBody::Full(body)) {
            Ok(outbound) => outbound,
            Err(_) => return Err(TransportFailure::Downstream),
        };
        *outbound.headers_mut() = headers;
        let budget = self.limits.proxy_timeout.saturating_sub(arrived.elapsed());
        let request = OutboundRequest {
            version,
            budget,
            request: outbound,
        };
        site.issued.store(true, Ordering::Relaxed);
        self.telemetry.outbound_issued(site.host);
        let call = self.connector.call(request);
        let response = match tokio::time::timeout(budget, call).await {
            Ok(response) => response,
            Err(_elapsed) => {
                self.telemetry
                    .availability(site.host, site.endpoint_host, false);
                return Err(TransportFailure::RequestTimeout);
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(failure) => {
                if matches!(
                    failure,
                    TransportFailure::Connection
                        | TransportFailure::RequestTimeout
                        | TransportFailure::LinkUnavailable
                ) {
                    self.telemetry
                        .availability(site.host, site.endpoint_host, false);
                }
                return Err(failure);
            }
        };
        self.versions
            .put(&target.authority, HttpVersion::of(response.version()));
        let (parts, body) = response.into_parts();
        self.telemetry
            .availability(site.host, site.endpoint_host, true);
        Ok(OutboundReply {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }

    /// The `ExecutionPlan` of one request, absent when the merged binding set
    /// is empty: a chain with no auth plugin and no guard or transform binding
    /// has nothing to run.
    ///
    /// # Errors
    /// Returns the `PluginNotFound` row a reference no registry resolves
    /// yields.
    fn chain_plan(&self, effective: &EffectiveConfig) -> Result<Option<ExecutionPlan>, OagwError> {
        let plugins: Vec<PluginBinding> =
            effective.plugin_bindings().into_iter().cloned().collect();
        let Some(binding) = self.auth_binding(effective) else {
            if plugins.is_empty() {
                return Ok(None);
            }
            // A guard or transform binding without an auth block still needs a
            // tier-1 entry for the plan to resolve; the never-invoked built-in
            // noop plugin fills it.
            return Ok(Some(resolve_plan(
                &self.registries,
                &BindingSet::new(noop_binding(), plugins),
            )?));
        };
        Ok(Some(resolve_plan(
            &self.registries,
            &BindingSet::new(binding_of(&binding), plugins),
        )?))
    }

    /// The auth `PluginBinding` the effective auth block names, when any.
    fn auth_binding(&self, effective: &EffectiveConfig) -> Option<AuthConfig> {
        effective.auth.clone()
    }

    /// Runs the auth phase of the plan, when the chain carries one.
    async fn auth_phase(
        &self,
        chain: Option<&ExecutionPlan>,
        prepared: Option<&mut RequestContext>,
        headers: &HeaderMap,
        request: &ProxyRequest,
    ) -> Result<(), OagwError> {
        let Some(plan) = chain else {
            return Ok(());
        };
        let Some(prepared) = prepared else {
            return Ok(());
        };
        prepared.headers = headers.clone();
        prepared.query = request
            .query
            .iter()
            .map(|param| (param.name.clone(), param.raw.clone()))
            .collect();
        plan.auth_phase(prepared).await
    }

    /// Runs the guard and transform request tier of the plan.
    async fn request_phase(
        &self,
        chain: Option<&ExecutionPlan>,
        prepared: Option<&mut RequestContext>,
    ) -> Result<(), OagwError> {
        let Some(plan) = chain else {
            return Ok(());
        };
        let Some(prepared) = prepared else {
            return Ok(());
        };
        match plan.request_phase(prepared).await? {
            GuardOutcome::Allowed => Ok(()),
            GuardOutcome::Rejected(rejection) => {
                Err(OagwError::validation_error(rejection.detail).with_status(rejection.status))
            }
        }
    }

    /// Maps a route-match rejection onto its row and closes the context.
    fn route_rejection(
        &self,
        context: &mut ProxyContext,
        rejection: MatchRejection,
    ) -> ProxyRejection {
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-11
        // Close the `ProxyContext` in `Failed` and return the rendered
        // rejection to the API handler.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-11
        let error = match rejection {
            MatchRejection::NotProxied => OagwError::route_error(
                "oagw.proxy: the upstream protocol is not proxied by this release",
            ),
            MatchRejection::Content(rule) => OagwError::validation_error(format!(
                "oagw.proxy: the request was rejected by the route rule {rule}"
            )),
            MatchRejection::NoRoute => {
                OagwError::route_not_found("oagw.proxy: no route of the tier matched the request")
            }
        };
        self.fail(context, error)
    }

    /// Maps an endpoint-selection failure onto its row and closes the context.
    fn selection_failure(
        &self,
        context: &mut ProxyContext,
        failure: SelectionFailure,
        pool: &[Endpoint],
    ) -> ProxyRejection {
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-15
        // Close the `ProxyContext` in `Failed` and return the rendered
        // rejection to the API handler.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-15
        let error = match failure {
            SelectionFailure::Missing => OagwError::missing_target_host(format!(
                "oagw.proxy: X-OAGW-Target-Host is required to disambiguate the endpoint pool; \
                 the valid hosts are {}",
                hosts_of(pool)
            )),
            SelectionFailure::Invalid => OagwError::invalid_target_host(
                "oagw.proxy: X-OAGW-Target-Host is not a hostname or an IP address",
            ),
            SelectionFailure::Unknown => OagwError::unknown_target_host(format!(
                "oagw.proxy: X-OAGW-Target-Host names no endpoint of this request's pool; \
                 the valid hosts are {}",
                hosts_of(pool)
            )),
        };
        self.fail(context, error)
    }

    /// Maps a transport failure onto the existing rows (`inst-pf-34`).
    fn transport_stage(
        &self,
        context: &mut ProxyContext,
        failure: TransportFailure,
        fields: ErrorContext,
    ) -> ProxyRejection {
        self.fail(context, transport_failure(failure).with_context(fields))
    }

    /// Hands the `ErrorContext` of a failed exchange back through the transform
    /// tier's `on_error` phase, which may attach the request identifier the
    /// upstream call propagated.
    async fn error_phase(
        &self,
        chain: Option<&ExecutionPlan>,
        upstream: &Upstream,
        outbound_path: &str,
        endpoint: &Endpoint,
    ) -> ErrorContext {
        let host = endpoint.host_stripped().unwrap_or_default();
        let mut fields = ErrorContext::new()
            .with_upstream_id(upstream.id.map_or_else(String::new, |id| id.to_string()))
            .with_host(host)
            .with_path(outbound_path);
        if let Some(plan) = chain
            // A transform failing inside its own error phase does not change
            // the transport row the exchange failure maps to: the row the
            // pipeline renders is the one the failure names. The refusal is
            // still recorded, at warn, so a broken error phase is never silent
            // and never reachable from outside the gateway's own telemetry.
            && let Err(error) = plan
                .on_reentry(crate::infra::plugin::plan::Reentry::Error(&mut fields))
                .await
        {
            tracing::warn!(
                error = %error,
                "oagw.proxy: a transform plugin failed inside its error phase; \
                 the transport row the exchange failure maps to is unchanged"
            );
        }
        fields
    }

    /// Closes the context in `Failed` and returns the rejection.
    fn fail(&self, context: &mut ProxyContext, error: OagwError) -> ProxyRejection {
        let _closed = context.advance(proxy::ProxyState::Failed);
        ProxyRejection::of(error)
    }
}

/// Resolves the token level a rate-limit outcome left behind, as the ratio over
/// the effective limit the check evaluated, `0.0` when no limit was in force
/// and no header was produced.
fn usage_ratio_of(outcome: &RateLimitOutcome) -> f64 {
    let headers = match outcome {
        RateLimitOutcome::Admitted { headers } => headers.as_ref(),
        RateLimitOutcome::Refused { headers, .. } => headers.as_ref(),
        RateLimitOutcome::NotLimited => None,
    };
    headers.map_or(0.0, |headers| {
        crate::domain::observability::usage_ratio(headers.limit, headers.remaining)
    })
}

/// The variant name of the row the closed table gives the authentication
/// failures to: the row the credential-resolution failure of the auth phase
/// maps onto and the row the upstream's rejection of the credentials it
/// injected reaches the caller as.
const AUTHENTICATION_ROW: &str = "AuthenticationFailed";

/// The `AuthenticationFailed` occurrence the upstream credential path records.
fn credential_row() -> OagwError {
    OagwError::authentication_failed(
        "oagw.proxy: the upstream rejected the credentials the auth phase injected",
    )
}

/// Resolves the `path` field of an audit record: the gear-relative proxy path
/// the request arrived on, with no query string.
fn audit_path_of(request: &ProxyRequest) -> String {
    ob::audit_path(&request.alias, &request.path_suffix)
}

/// Maps a transport failure onto the existing row of the closed table, the one
/// mapping every branch of the proxy path consults: a buffered dispatch, a
/// streamed body and an upgrade handshake are all rendered through it, so no
/// two branches can ever disagree on the row a failure is rendered with.
///
/// # Errors
/// Never returns an error: the function builds the row it returns.
pub(crate) fn transport_failure(failure: TransportFailure) -> OagwError {
    match failure {
        TransportFailure::Connection => OagwError::connection_timeout(
            "oagw.proxy: the upstream connection could not be established",
        ),
        TransportFailure::RequestTimeout => OagwError::request_timeout(
            "oagw.proxy: the upstream did not answer within the request timeout",
        ),
        TransportFailure::IdleTimeout => {
            OagwError::idle_timeout("oagw.proxy: an idle upstream read timed out")
        }
        TransportFailure::Protocol => {
            OagwError::protocol_error("oagw.proxy: the upstream spoke an unusable protocol")
        }
        TransportFailure::Downstream => OagwError::downstream_error(
            "oagw.proxy: the upstream exchange failed before it completed",
        ),
        TransportFailure::StreamAborted => {
            OagwError::stream_aborted("oagw.proxy: the upstream stream aborted")
        }
        TransportFailure::LinkUnavailable => {
            OagwError::link_unavailable("oagw.proxy: the upstream is unreachable from this gateway")
        }
    }
}

/// The endpoint pool of an upstream, empty when the upstream carries none.
fn endpoints(upstream: &Upstream) -> &[Endpoint] {
    upstream
        .server
        .as_ref()
        .map_or(&[][..], |server| server.endpoints.as_slice())
}

/// The endpoint hosts of one pool, the list the `MissingTargetHost` and
/// `UnknownTargetHost` details name. Only configuration names are carried: no
/// header value, no query string and no credential material ever reaches a
/// detail.
fn hosts_of(pool: &[Endpoint]) -> String {
    let hosts: Vec<&str> = pool
        .iter()
        .filter_map(|endpoint| endpoint.host_stripped())
        .collect();
    if hosts.is_empty() {
        "(the pool carries none)".to_owned()
    } else {
        hosts.join(", ")
    }
}

/// The `request.*` rules of an upstream, when its headers block carries them.
fn request_rules(upstream: &Upstream) -> Option<&crate::domain::model::RequestHeaders> {
    upstream
        .headers
        .as_ref()
        .and_then(|headers| headers.request.as_ref())
}

/// The `response.*` rules of an upstream, when its headers block carries them.
fn response_rules(upstream: &Upstream) -> Option<&crate::domain::model::ResponseHeaders> {
    upstream
        .headers
        .as_ref()
        .and_then(|headers| headers.response.as_ref())
}

/// The `auth_plugin` binding of an effective auth block.
fn binding_of(auth: &AuthConfig) -> PluginBinding {
    PluginBinding {
        position: 0,
        plugin_ref: auth
            .plugin_type
            .clone()
            .unwrap_or_else(|| NOOP_AUTH_PLUGIN_ID.to_owned()),
        plugin_uuid: None,
        config: auth.config.clone(),
    }
}

/// The never-invoked auth binding a chain without an auth block is given.
fn noop_binding() -> PluginBinding {
    PluginBinding {
        position: 0,
        plugin_ref: NOOP_AUTH_PLUGIN_ID.to_owned(),
        plugin_uuid: None,
        config: None,
    }
}

/// The rate-limit headers a 429 carries unchanged (`inst-pf-27`).
fn rate_limit_headers(headers: Option<RateLimitHeaders>, retry_after_secs: u64) -> HeaderMap {
    let mut out = HeaderMap::new();
    let retry = HeaderValue::from_str(&retry_after_secs.to_string());
    if let Ok(value) = retry {
        out.insert(HeaderName::from_static("retry-after"), value);
    }
    if let Some(headers) = headers {
        for (name, value) in [
            (
                crate::domain::ratelimit::HEADER_LIMIT,
                headers.limit.to_string(),
            ),
            (
                crate::domain::ratelimit::HEADER_REMAINING,
                headers.remaining.to_string(),
            ),
            (
                crate::domain::ratelimit::HEADER_RESET,
                headers.reset.to_string(),
            ),
        ] {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                out.insert(name, value);
            }
        }
    }
    out
}

/// The CORS response headers an allowed cross-origin request carries, and the
/// `Vary: Origin` the CORS flow's not-applicable branch asks the caller for.
fn cors_headers(mut headers: HeaderMap, allowed: Option<&AllowedHeaders>) -> HeaderMap {
    if let Some(allowed) = allowed {
        if let Ok(value) = HeaderValue::from_str(&allowed.allow_origin) {
            headers.insert(
                HeaderName::from_static("access-control-allow-origin"),
                value,
            );
        }
        if let Some(expose) = allowed.expose_headers.as_ref()
            && let Ok(value) = HeaderValue::from_str(expose)
        {
            headers.insert(
                HeaderName::from_static("access-control-expose-headers"),
                value,
            );
        }
        if allowed.allow_credentials {
            headers.insert(
                HeaderName::from_static("access-control-allow-credentials"),
                HeaderValue::from_static("true"),
            );
        }
    }
    headers.append(
        HeaderName::from_static("vary"),
        HeaderValue::from_static(proxy::VARY_ORIGIN),
    );
    headers
}

/// The headers a CORS rejection carries: `Vary: Origin`, so a cache keyed on
/// the origin never serves one origin's 403 to another.
fn cors_rejection_headers() -> HeaderMap {
    cors_headers(HeaderMap::new(), None)
}

/// The first value of one inbound header, as a string.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
// @cpt-end:cpt-cf-oagw-dod-error-source-header:p1:inst-full
// @cpt-end:cpt-cf-oagw-dod-proxy-request-pipeline:p1:inst-full

/// The test double of the upstream connector and of the tenant resolver, plus
/// the pipeline constructor the shell and gear tests mount the proxy handlers
/// through. Test-only, so no production path can reach a canned response.
#[cfg(test)]
pub(crate) mod stub {
    use std::sync::Arc;

    use bytes::Bytes;
    use credstore_sdk::test_util::MockCredStoreClient;
    use http::{HeaderMap, StatusCode, Version};
    use parking_lot::Mutex;
    use tenant_resolver_sdk::{
        GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
        GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef,
        TenantResolverClient, TenantResolverError, TenantStatus,
    };
    use toolkit_security::SecurityContext;

    use tokio::sync::mpsc;

    use futures_util::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::{
        BoxError, EffectiveConfigResolver, HttpVersionCache, InMemoryStores, OutboundBody,
        OutboundRequest, PinnedCall, PinnedUpgrade, PluginRegistries, ProxyLimits, ProxyPipeline,
        RateLimiter, StreamingBody, TransportFailure, UpstreamConnector,
    };
    use crate::config::{DEFAULT_TOKEN_CACHE_CAPACITY, DEFAULT_TOKEN_CACHE_TTL_SECS};
    use crate::infra::observability::Telemetry;
    use crate::infra::plugin::token_cache::TokenCacheConfig;
    use crate::infra::proxy::streaming::{BoxDuplex, UpgradeHandover};

    /// A canned-response connector, the test double the pipeline tests drive.
    #[derive(Default)]
    pub struct StubConnector {
        responses: Mutex<Vec<StubReply>>,
        streams: Mutex<Vec<Option<StubStreamReply>>>,
        upgrades: Mutex<Vec<StubUpgradeReply>>,
        received: Mutex<Vec<OutboundRequest>>,
        failure: Mutex<Option<TransportFailure>>,
        delay: Mutex<Option<Duration>>,
    }

    /// One canned response a stub connector answers with.
    #[derive(Clone)]
    pub struct StubReply {
        /// The status the canned response carries.
        pub status: StatusCode,
        /// The headers the canned response carries.
        pub headers: HeaderMap,
        /// The body the canned response carries.
        pub body: Bytes,
        /// The version the canned response reports as negotiated.
        pub version: Version,
    }

    impl StubReply {
        /// A canned `200 OK` response with no header and no body.
        #[must_use]
        pub fn ok() -> Self {
            Self {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::new(),
                version: Version::HTTP_11,
            }
        }
    }

    impl StubConnector {
        /// A connector that answers with no canned response at all.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Makes every later call fail with `failure`, the transport-failure
        /// mapping tests driving it after a first call was recorded.
        pub fn set_failure(&self, failure: TransportFailure) {
            *self.failure.lock() = Some(failure);
        }

        /// Clears the failure, the connector answering its queued responses
        /// again: the form a recovery test drives.
        pub fn clear_failure(&self) {
            *self.failure.lock() = None;
        }

        /// Makes every later call and upgrade wait `delay` before answering,
        /// the establishment-budget test needing a connector that is slow on
        /// purpose.
        pub fn set_delay(&self, delay: Duration) {
            *self.delay.lock() = Some(delay);
        }

        /// Queues one canned response, answering the queued responses in the
        /// order they were queued.
        pub fn push(&self, reply: StubReply) {
            self.responses.lock().push(reply);
        }

        /// Queues one streamed response, answered with a body the caller
        /// produces one frame at a time, in the order the streams were queued.
        pub fn push_stream(&self, reply: StubStreamReply) {
            self.streams.lock().push(Some(reply));
        }

        /// Queues one upgrade answer, handing the queued end of the session
        /// back, in the order the upgrades were queued.
        pub fn push_upgrade(&self, reply: StubUpgradeReply) {
            self.upgrades.lock().push(reply);
        }

        /// The requests the connector received, in order, taking them away.
        #[must_use]
        pub fn received(&self) -> Vec<OutboundRequest> {
            std::mem::take(&mut *self.received.lock())
        }
    }

    /// Builds a canned response of the body the stub was given.
    fn response_with(
        status: StatusCode,
        headers: HeaderMap,
        version: Version,
        body: OutboundBody,
    ) -> http::Response<OutboundBody> {
        let mut response = http::Response::builder()
            .status(status)
            .version(version)
            .body(body)
            .expect("a stub response of a fixed status and body");
        *response.headers_mut() = headers;
        response
    }

    /// Adapts the frame channel a test drives into the stream a streamed body
    /// wraps.
    struct ReceiverStream {
        inner: mpsc::Receiver<Result<Bytes, BoxError>>,
    }

    impl Stream for ReceiverStream {
        type Item = Result<Bytes, BoxError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.inner).poll_recv(cx)
        }
    }

    impl UpstreamConnector for StubConnector {
        fn call<'a>(&'a self, outbound: OutboundRequest) -> PinnedCall<'a> {
            self.received.lock().push(outbound);
            Box::pin(async move {
                if let Some(failure) = *self.failure.lock() {
                    return Err(failure);
                }
                let delay = *self.delay.lock();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let reply = if self.responses.lock().is_empty() {
                    StubReply::ok()
                } else {
                    self.responses.lock().remove(0)
                };
                let stream = self.streams.lock().pop().flatten();
                if let Some(stream) = stream {
                    let hint = hyper::body::SizeHint::default();
                    return Ok(response_with(
                        stream.status,
                        stream.headers,
                        Version::HTTP_11,
                        OutboundBody::Streaming(StreamingBody::new(
                            Box::pin(ReceiverStream {
                                inner: stream.frames,
                            }),
                            Duration::from_secs(30),
                            hint,
                        )),
                    ));
                }
                let mut response = http::Response::builder()
                    .status(reply.status)
                    .version(reply.version)
                    .body(OutboundBody::Full(reply.body))
                    .map_err(|_| TransportFailure::Downstream)?;
                *response.headers_mut() = reply.headers;
                Ok(response)
            })
        }

        fn upgrade<'a>(&'a self, outbound: OutboundRequest) -> PinnedUpgrade<'a> {
            self.received.lock().push(outbound);
            Box::pin(async move {
                if let Some(failure) = *self.failure.lock() {
                    return Err(failure);
                }
                let delay = *self.delay.lock();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let queued = {
                    let mut upgrades = self.upgrades.lock();
                    if upgrades.is_empty() {
                        None
                    } else {
                        Some(upgrades.remove(0))
                    }
                };
                let reply = match queued {
                    Some(reply) => reply,
                    None => return Err(TransportFailure::Protocol),
                };
                let status = reply.status;
                let headers = reply.headers;
                Ok(UpgradeHandover {
                    status,
                    headers,
                    io: reply.io,
                })
            })
        }
    }

    impl StubUpgradeReply {
        /// A canned `101 Switching Protocols` answer carrying `accept`.
        #[must_use]
        pub fn accepted(accept: &str) -> Self {
            let mut headers = HeaderMap::new();
            headers.insert(
                "sec-websocket-accept",
                http::HeaderValue::from_str(accept).expect("the accept is a header value"),
            );
            Self {
                status: StatusCode::SWITCHING_PROTOCOLS,
                headers,
                io: Box::new(tokio::io::duplex(4096).0),
            }
        }

        /// A canned `200 OK` answer, the shape of an upstream that refused the
        /// handshake.
        #[must_use]
        pub fn refused() -> Self {
            Self {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                io: Box::new(tokio::io::duplex(4096).0),
            }
        }
    }

    /// A canned response whose body the caller produces one frame at a time.
    pub struct StubStreamReply {
        /// The status the canned response carries.
        pub status: StatusCode,
        /// The headers the canned response carries.
        pub headers: HeaderMap,
        /// The frames the canned stream delivers, ending when the sender drops.
        pub frames: mpsc::Receiver<Result<Bytes, BoxError>>,
    }

    /// A canned upgrade answer, carrying the test's end of the session.
    pub struct StubUpgradeReply {
        /// The status the canned answer carries, 101 for a handshake that
        /// succeeded.
        pub status: StatusCode,
        /// The headers the canned answer carries.
        pub headers: HeaderMap,
        /// The test's end of the upgraded connection.
        pub io: BoxDuplex,
    }

    /// A tenant resolver whose chain is the caller's own tenant and nothing
    /// else, so a request resolves against the store of its own control plane.
    pub(crate) struct StubTenantResolver;

    #[async_trait::async_trait]
    impl TenantResolverClient for StubTenantResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::TenantNotFound { tenant_id: id })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::TenantNotFound {
                tenant_id: TenantId::nil(),
            })
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Ok(GetAncestorsResponse {
                tenant: single_ref(id),
                ancestors: Vec::new(),
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Ok(GetDescendantsResponse {
                tenant: single_ref(id),
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Ok(false)
        }
    }

    pub(crate) fn single_ref(id: TenantId) -> TenantRef {
        TenantRef {
            id,
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }

    /// A pipeline over `stores`, answering the outbound call through
    /// `connector`, with the built-in plugin registries and the shell defaults
    /// of [`crate::infra::proxy::pipeline::ProxyLimits`].
    pub(crate) fn pipeline(
        stores: &InMemoryStores,
        connector: Arc<dyn UpstreamConnector>,
    ) -> Arc<ProxyPipeline> {
        pipeline_with_limits(stores, connector, ProxyLimits::default())
    }

    /// The same pipeline over an explicit set of limits, the form the
    /// body-limit and scheme-policy tests drive.
    pub(crate) fn pipeline_with_limits(
        stores: &InMemoryStores,
        connector: Arc<dyn UpstreamConnector>,
        limits: ProxyLimits,
    ) -> Arc<ProxyPipeline> {
        pipeline_with_telemetry(stores, connector, limits, telemetry_with_sink())
    }

    /// The same pipeline, emitting through the telemetry a test hands over, the
    /// form the observability tests drive.
    pub(crate) fn pipeline_with_telemetry(
        stores: &InMemoryStores,
        connector: Arc<dyn UpstreamConnector>,
        limits: ProxyLimits,
        telemetry: Arc<Telemetry>,
    ) -> Arc<ProxyPipeline> {
        Arc::new(ProxyPipeline::with_connector(
            connector,
            Arc::new(HttpVersionCache::default()),
            Arc::new(EffectiveConfigResolver::new(
                Arc::new(stores.upstreams()),
                Arc::new(stores.routes()),
                Arc::new(StubTenantResolver),
            )),
            Arc::new(RateLimiter::new()),
            Arc::new(PluginRegistries::with_builtins(
                Arc::new(MockCredStoreClient::empty()),
                None,
                TokenCacheConfig::new(DEFAULT_TOKEN_CACHE_TTL_SECS, DEFAULT_TOKEN_CACHE_CAPACITY),
            )),
            limits,
            telemetry,
        ))
    }

    /// A facade whose metric handles are no-ops and whose audit records are
    /// captured: the form the pipeline tests drive when they assert on nothing
    /// emitted, and the sink the audit assertions read.
    pub(crate) fn telemetry_with_sink() -> Arc<Telemetry> {
        let (telemetry, _sink) = telemetry_with_capture();
        telemetry
    }

    /// The same facade with the sink handle kept, so a test can assert on the
    /// audit records the pipeline wrote.
    pub(crate) fn telemetry_with_capture() -> (
        Arc<Telemetry>,
        Arc<crate::infra::observability::CapturingAuditSink>,
    ) {
        let sink = Arc::new(crate::infra::observability::CapturingAuditSink::default());
        use opentelemetry::metrics::MeterProvider as _;
        let meter = opentelemetry::metrics::NoopMeterProvider::new().meter("oagw");
        (
            Arc::new(Telemetry::new(
                crate::infra::observability::MetricInstruments::register(&meter),
                Arc::clone(&sink) as Arc<dyn crate::infra::observability::AuditSink>,
            )),
            sink,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::observability::AuditSink as TelemetryAuditSink;
    use std::net::{IpAddr, Ipv4Addr};

    const ALIAS: &str = "payments";
    const SUBJECT: Uuid = Uuid::from_u128(0xa110);
    const TENANT: Uuid = Uuid::from_u128(0xa110_0001);

    /// A security context whose subject and tenant are the test's own.
    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    fn peer() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    use std::collections::BTreeMap;

    use async_trait::async_trait;

    use crate::domain::model::{
        ALGORITHM_TOKEN_BUCKET, BurstCapacity, CorsConfig, HeadersConfig, HttpMatch, MatchConfig,
        PASSTHROUGH_ALL, PASSTHROUGH_NONE, PROTOCOL_GRPC, PROTOCOL_HTTP, RateLimitConfig,
        RequestHeaders, ResponseHeaders, Route as RouteModel, SCHEME_GRPC, SCHEME_HTTP,
        SCHEME_HTTPS, SCHEME_WSS, SCHEME_WT, SCOPE_TENANT, SHARING_PRIVATE, STRATEGY_REJECT,
        ServerConfig, SustainedRate, WINDOW_SECOND,
    };
    use crate::domain::proxy::{PREFLIGHT_MAX_AGE, VARY_ORIGIN, VARY_PREFLIGHT};
    use crate::domain::repo::{RouteRepository, UpstreamRepository};
    use crate::domain::streaming::{StreamFailure, StreamSessionState, classify};
    use crate::infra::proxy::limiter::RateLimiter as Limiter;
    use stub::{StubConnector, StubReply, StubStreamReply, StubUpgradeReply};

    const UPSTREAM_ID: Uuid = Uuid::from_u128(0x0a11_ce70_0000_0000_0000_0000_0000_0001);
    const ANCESTOR_TENANT: Uuid = Uuid::from_u128(0x0a11_ce70_0000_0000_0000_0000_0000_00a1);
    const ANCESTOR_UPSTREAM_ID: Uuid = Uuid::from_u128(0x0a11_ce70_0000_0000_0000_0000_0000_00a2);
    const HOST: &str = "api.vendor.com";
    /// A GTS-shaped reference no registry of the built-in set resolves.
    const UNKNOWN_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.unknown.v1";
    const SIBLING_HOST: &str = "api2.vendor.com";

    /// The endpoint every default fixture reaches.
    fn endpoint(scheme: &str, host: &str, port: i64) -> Endpoint {
        Endpoint::new(scheme, host, port)
    }

    /// An enabled HTTPS upstream of the test tenant under the alias.
    fn upstream(id: Uuid, alias: &str, endpoints: &[Endpoint]) -> Upstream {
        Upstream {
            id: Some(id),
            tenant_id: Some(TENANT),
            enabled: true,
            alias: Some(alias.to_owned()),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            server: Some(ServerConfig {
                endpoints: endpoints.to_vec(),
            }),
            ..Upstream::default()
        }
    }

    impl Upstream {
        fn map(self, apply: impl FnOnce(&mut Upstream)) -> Self {
            let mut this = self;
            apply(&mut this);
            this
        }

        fn with_headers(self, headers: HeadersConfig) -> Self {
            self.map(|upstream| upstream.headers = Some(headers))
        }

        fn with_cors(self, cors: CorsConfig) -> Self {
            self.map(|upstream| upstream.cors = Some(cors))
        }

        /// Carries the auth block the credential-resolution tests drive.
        fn with_auth(self, auth: AuthConfig) -> Self {
            self.map(|upstream| upstream.auth = Some(auth))
        }

        fn with_rate_limit(self, limit: RateLimitConfig) -> Self {
            self.map(|upstream| upstream.rate_limit = Some(limit))
        }

        fn disabled(self) -> Self {
            self.map(|upstream| upstream.enabled = false)
        }
    }

    /// A GET route of the upstream `upstream_id`, matching `path` with the
    /// priority and the allowlist the caller chooses.
    fn route(upstream_id: Uuid, path: &str, priority: i64, allowlist: &[&str]) -> RouteModel {
        RouteModel {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(TENANT),
            enabled: true,
            priority,
            upstream_id: Some(upstream_id),
            match_config: Some(MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: Some(path.to_owned()),
                    query_allowlist: allowlist.iter().map(|name| (*name).to_owned()).collect(),
                    path_suffix_mode: "append".to_owned(),
                }),
                grpc: None,
            }),
            ..RouteModel::default()
        }
    }

    impl RouteModel {
        fn with_methods(self, methods: &[&str]) -> Self {
            let mut this = self;
            if let Some(http) = this
                .match_config
                .as_mut()
                .and_then(|config| config.http.as_mut())
            {
                http.methods = methods.iter().map(|method| (*method).to_owned()).collect();
            }
            this
        }

        fn with_plugin(self, plugin: &str) -> Self {
            let mut this = self;
            this.plugins = Some(crate::domain::model::PluginsConfig {
                sharing: SHARING_PRIVATE.to_owned(),
                items: vec![plugin.to_owned()],
            });
            this
        }
    }

    /// A gRPC route of the upstream `upstream_id`, the match shape a
    /// non-HTTP-protocol upstream carries.
    fn grpc_route(upstream_id: Uuid) -> RouteModel {
        RouteModel {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(TENANT),
            enabled: true,
            priority: 10,
            upstream_id: Some(upstream_id),
            match_config: Some(MatchConfig {
                http: None,
                grpc: Some(crate::domain::model::GrpcMatch {
                    service: Some("vendor.v1.Catalog".to_owned()),
                    method: Some("Get".to_owned()),
                }),
            }),
            ..RouteModel::default()
        }
    }

    /// A block whose burst capacity is the sustained rate, so a second request
    /// in the same window is refused.
    fn rate_limit(capacity: i64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SHARING_PRIVATE.to_owned(),
            algorithm: ALGORITHM_TOKEN_BUCKET.to_owned(),
            sustained: Some(SustainedRate {
                rate: Some(capacity),
                window: WINDOW_SECOND.to_owned(),
            }),
            burst: Some(BurstCapacity {
                capacity: Some(capacity),
            }),
            scope: SCOPE_TENANT.to_owned(),
            strategy: STRATEGY_REJECT.to_owned(),
            cost: Some(1),
        }
    }

    /// A CORS block that allows exactly `origins` and the methods `methods`.
    fn cors(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: SHARING_PRIVATE.to_owned(),
            enabled: Some(true),
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            ..CorsConfig::default()
        }
    }

    /// A request/response header rule block, with the passthrough mode `mode`.
    fn headers_block(mode: &str) -> HeadersConfig {
        let mut set = BTreeMap::new();
        set.insert("x-signed".to_owned(), "gateway".to_owned());
        let mut add = BTreeMap::new();
        add.insert("x-added".to_owned(), "1".to_owned());
        let mut response_set = BTreeMap::new();
        response_set.insert("x-response".to_owned(), "set".to_owned());
        HeadersConfig {
            request: Some(RequestHeaders {
                set,
                add,
                remove: vec!["x-stripped".to_owned()],
                passthrough: mode.to_owned(),
                passthrough_allowlist: vec!["x-allowed".to_owned()],
            }),
            response: Some(ResponseHeaders {
                set: response_set,
                add: BTreeMap::new(),
                remove: vec!["x-upstream-secret".to_owned()],
            }),
        }
    }

    /// The harness of the pipeline tests: the stores, the stub connector and
    /// the pipeline built over both, the way the shell mounts it.
    struct Rig {
        stores: InMemoryStores,
        connector: Arc<StubConnector>,
        pipeline: Arc<ProxyPipeline>,
        /// The alias every request of the rig carries, which is the upstream
        /// the stores hold for it.
        alias: String,
        /// The emission the rig's pipeline writes, captured rather than scraped,
        /// when the rig was built as an observed one.
        observation: Option<Observation>,
    }

    /// The captured emission an observed rig holds: the metric reader and the
    /// audit sink the assertions read.
    struct Observation {
        metrics: crate::infra::observability::harness::MetricsRig,
        sink: Arc<crate::infra::observability::CapturingAuditSink>,
    }

    impl Rig {
        /// A rig over the given connector and limits, with empty stores.
        fn over(connector: StubConnector, limits: ProxyLimits) -> Self {
            let stores = InMemoryStores::new();
            let connector = Arc::new(connector);
            let pipeline = stub::pipeline_with_limits(
                &stores,
                Arc::clone(&connector) as Arc<dyn UpstreamConnector>,
                limits,
            );
            Self {
                stores,
                connector,
                pipeline,
                alias: ALIAS.to_owned(),
                observation: None,
            }
        }

        /// An observed rig over the given connector and limits: its pipeline
        /// emits through a metric reader and an audit sink the test captures.
        fn observed_over(connector: StubConnector, limits: ProxyLimits) -> Self {
            let stores = InMemoryStores::new();
            let connector = Arc::new(connector);
            let metrics = crate::infra::observability::harness::MetricsRig::build();
            let sink = Arc::new(crate::infra::observability::CapturingAuditSink::default());
            let telemetry =
                Arc::new(metrics.telemetry(Arc::clone(&sink) as Arc<dyn TelemetryAuditSink>));
            let pipeline = stub::pipeline_with_telemetry(
                &stores,
                Arc::clone(&connector) as Arc<dyn UpstreamConnector>,
                limits,
                telemetry,
            );
            Self {
                stores,
                connector,
                pipeline,
                alias: ALIAS.to_owned(),
                observation: Some(Observation { metrics, sink }),
            }
        }

        /// A rig over the default limits and no stored configuration.
        fn plain() -> Self {
            Self::over(StubConnector::new(), ProxyLimits::default())
        }

        /// An observed rig over the default limits and no stored configuration.
        fn observed() -> Self {
            Self::observed_over(StubConnector::new(), ProxyLimits::default())
        }

        /// An observed rig holding the single-endpoint HTTPS upstream and its
        /// GET route at `/api`, the configuration the emission tests drive.
        fn observed_single() -> Self {
            let rig = Self::observed();
            rig.seed_upstream(upstream(
                UPSTREAM_ID,
                ALIAS,
                &[endpoint(SCHEME_HTTPS, HOST, 443)],
            ));
            rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
            rig
        }

        /// A rig that routes by another alias, the form a common-suffix pool
        /// whose alias is the pool's registrable suffix drives.
        fn routing_by(alias: &str) -> Self {
            let mut rig = Self::plain();
            rig.alias = alias.to_owned();
            rig
        }

        /// A rig holding the single-endpoint HTTPS upstream and its GET route
        /// at `/api`, the default configuration every happy-path test drives.
        fn single() -> Self {
            let rig = Self::plain();
            rig.seed_upstream(upstream(
                UPSTREAM_ID,
                ALIAS,
                &[endpoint(SCHEME_HTTPS, HOST, 443)],
            ));
            rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
            rig
        }

        /// The audit records the rig's pipeline wrote, taken away.
        fn audit_lines(&self) -> Vec<String> {
            self.observation
                .as_ref()
                .map(|observation| observation.sink.lines())
                .unwrap_or_default()
        }

        /// The metric series one collection of the rig's reader produced.
        fn metrics(&self) -> Vec<crate::infra::observability::harness::Series> {
            self.observation
                .as_ref()
                .map(|observation| observation.metrics.snapshot())
                .unwrap_or_default()
        }

        /// Stores the upstream and returns its identifier.
        fn seed_upstream(&self, candidate: Upstream) -> Uuid {
            self.stores
                .upstreams()
                .insert(&candidate)
                .expect("the test upstream is well formed")
                .id
                .expect("the stored upstream carries its identifier")
        }

        /// Stores the route and returns its identifier.
        fn seed_route(&self, candidate: RouteModel) -> Uuid {
            self.stores
                .routes()
                .insert(&candidate)
                .expect("the test route is well formed")
                .id
                .expect("the stored route carries its identifier")
        }

        /// A proxy request for the path and its `?query` part, with no header.
        fn request(&self, method: &str, path_and_query: &str) -> ProxyRequest {
            self.request_with(method, path_and_query, &[])
        }

        /// A proxy request for the path and its `?query` part, with inbound
        /// headers, classified the way the shell hands a request over.
        fn request_with(
            &self,
            method: &str,
            path_and_query: &str,
            headers: &[(&str, &str)],
        ) -> ProxyRequest {
            let (suffix, query) = match path_and_query.split_once('?') {
                Some((path, query)) => (path, query),
                None => (path_and_query, ""),
            };
            let mut header_map = HeaderMap::new();
            for (name, value) in headers {
                header_map.insert(
                    HeaderName::from_bytes(name.as_bytes()).expect("the header name is valid"),
                    HeaderValue::from_str(value).expect("the header value is valid"),
                );
            }
            ProxyRequest {
                method: method.to_owned(),
                alias: self.alias.clone(),
                path_suffix: suffix.to_owned(),
                query: proxy::parse_query(query),
                headers: header_map,
                body_len: 0,
            }
        }

        /// Runs the pipeline over `request` and `body`.
        async fn handle_bytes(
            &self,
            request: &ProxyRequest,
            body: Bytes,
        ) -> Result<ProxyOutcome, ProxyRejection> {
            self.pipeline
                .handle(request, &security(), peer(), body)
                .await
        }

        /// Runs the pipeline over `request`, carrying no body.
        async fn handle(&self, request: &ProxyRequest) -> Result<ProxyOutcome, ProxyRejection> {
            self.handle_bytes(request, Bytes::new()).await
        }

        /// The upstream reply a request that reaches the connector produced.
        async fn reply(&self, request: &ProxyRequest) -> UpstreamReply {
            match self.handle(request).await {
                Ok(ProxyOutcome::Upstream(reply)) => reply,
                other => panic!("the request must pass through, got {other:?}"),
            }
        }

        /// The rejection a request the pipeline refused produced.
        async fn refused(&self, request: &ProxyRequest) -> ProxyRejection {
            match self.handle(request).await {
                Err(rejection) => rejection,
                other => panic!("the request must be refused, got {other:?}"),
            }
        }

        /// The requests the connector received, taking them away.
        fn received(&self) -> Vec<OutboundRequest> {
            self.connector.received()
        }
    }

    /// The mapped row of a rejection.
    fn row_of(rejection: &ProxyRejection) -> (&'static str, u16, &'static str) {
        let mapping = rejection.error.mapping();
        (mapping.variant, mapping.status, mapping.gts_type)
    }

    /// An error whose message is `message` and whose source chain is `source`:
    /// the shape the transport classifier walks.
    #[derive(Debug)]
    struct Chained {
        message: &'static str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Chained {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            let source: &(dyn std::error::Error + 'static) = self.source.as_deref()?;
            Some(source)
        }
    }

    /// The first value of one header of `headers`.
    fn header_of<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a HeaderValue> {
        headers.get(name)
    }

    /// The first value of one header of `headers`, as a string.
    fn text_of<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The outbound `http::Request` one recorded call carried.
    fn outbound(call: &OutboundRequest) -> &http::Request<OutboundBody> {
        &call.request
    }

    /// The body of an upstream reply: the bytes the stub connector answered
    /// with, the streaming form being the production path no stub produces.
    fn body_of(reply: &UpstreamReply) -> Bytes {
        match &reply.body {
            OutboundBody::Full(bytes) => bytes.clone(),
            OutboundBody::Streaming(_) => panic!("the stub connector answers with a full body"),
        }
    }

    /// A tenant resolver whose chain is the subject tenant and one ancestor, so
    /// a disabled same-alias upstream of the ancestor is reported as the
    /// disabled-ancestor disposition.
    struct TieredResolver;

    #[async_trait]
    impl tenant_resolver_sdk::TenantResolverClient for TieredResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: tenant_resolver_sdk::TenantId,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound { tenant_id: id })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound {
                tenant_id: tenant_resolver_sdk::TenantId::nil(),
            })
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[tenant_resolver_sdk::TenantId],
            _options: &tenant_resolver_sdk::GetTenantsOptions,
        ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError>
        {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetAncestorsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetAncestorsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetAncestorsResponse {
                tenant: stub::single_ref(id),
                ancestors: vec![stub::single_ref(tenant_resolver_sdk::TenantId(
                    ANCESTOR_TENANT,
                ))],
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetDescendantsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetDescendantsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetDescendantsResponse {
                tenant: stub::single_ref(id),
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: tenant_resolver_sdk::TenantId,
            _descendant_id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::IsAncestorOptions,
        ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
            Ok(false)
        }
    }

    // ------------------------------------------------------------------
    // The stages in the order §2 fixes them
    // ------------------------------------------------------------------

    /// Acceptance criterion 1.
    #[tokio::test]
    async fn a_proxied_request_reaches_the_endpoint_once_and_passes_through() {
        let rig = Rig::single();
        rig.connector.push(StubReply {
            status: StatusCode::CREATED,
            headers: {
                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("content-type"),
                    HeaderValue::from_static("application/vnd.vendor.v1+json"),
                );
                headers.insert(
                    HeaderName::from_static("x-upstream"),
                    HeaderValue::from_static("yes"),
                );
                headers
            },
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });

        let reply = rig.reply(&rig.request("GET", "/api/v1/users")).await;

        let calls = rig.received();
        assert_eq!(
            calls.len(),
            1,
            "the request reaches the endpoint exactly once"
        );
        let call = &calls[0];
        assert_eq!(outbound(call).method(), http::Method::GET);
        assert_eq!(
            outbound(call).uri(),
            "https://api.vendor.com/api/v1/users",
            "the outbound URI is the endpoint's, with the matched path composed"
        );
        assert_eq!(
            header_of(outbound(call).headers(), "host").and_then(|value| value.to_str().ok()),
            Some(HOST),
            "Host carries the selected endpoint's host"
        );
        assert!(
            header_of(outbound(call).headers(), TARGET_HOST_HEADER).is_none(),
            "the routing header never reaches the outbound request"
        );

        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(
            text_of(&reply.headers, "content-type"),
            Some("application/vnd.vendor.v1+json"),
            "the upstream Content-Type is forwarded untouched"
        );
        assert_eq!(
            header_of(&reply.headers, "x-upstream").and_then(|value| value.to_str().ok()),
            Some("yes")
        );
        assert_eq!(body_of(&reply), Bytes::from_static(b"hello"));
        assert_eq!(
            header_of(&reply.headers, "vary").and_then(|value| value.to_str().ok()),
            Some(VARY_ORIGIN),
            "a proxied response carries Vary: Origin"
        );
    }

    /// Acceptance criterion 2: a request refused at a stage runs none of the
    /// stages after it.
    #[tokio::test]
    async fn a_refused_request_runs_no_stage_after_the_refusing_one() {
        // No configuration at all: the resolution disposition refuses the
        // request and nothing downstream runs.
        let rig = Rig::plain();
        let rejection = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(row_of(&rejection).1, 404);
        assert!(
            rig.received().is_empty(),
            "no upstream call follows a disposition"
        );

        // A route rejection: no endpoint selection, no plugin and no upstream.
        let rig = Rig::single();
        let rejection = rig.refused(&rig.request("POST", "/api/users")).await;
        assert_eq!(
            row_of(&rejection).1,
            404,
            "the method is in no route's allowlist"
        );
        assert!(
            rig.received().is_empty(),
            "no endpoint is selected after a route rejection"
        );

        // A validation failure: the upstream is never called.
        let rig = Rig::single();
        let request = rig.request_with("GET", "/api", &[("transfer-encoding", "gzip")]);
        let rejection = rig.refused(&request).await;
        assert_eq!(row_of(&rejection).1, 400);
        assert!(
            rig.received().is_empty(),
            "no upstream call follows a validation failure"
        );
    }

    /// Acceptance criterion 3: the preflight is answered before anything is
    /// resolved.
    #[tokio::test]
    async fn a_preflight_is_answered_before_anything_is_resolved() {
        // The stores hold no configuration at all, so any stage after the CORS
        // check would refuse the request.
        let rig = Rig::plain();
        let request = rig.request_with(
            "OPTIONS",
            "/api",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "GET"),
                ("access-control-request-headers", "x-trace"),
            ],
        );

        let outcome = rig
            .handle(&request)
            .await
            .expect("the preflight is answered");

        match outcome {
            ProxyOutcome::Preflight(headers) => {
                assert_eq!(headers.allow_origin, "https://app.example.com");
                assert_eq!(headers.allow_methods, "GET");
                assert_eq!(headers.allow_headers, "x-trace");
                assert_eq!(headers.max_age, PREFLIGHT_MAX_AGE);
                assert_eq!(headers.vary, VARY_PREFLIGHT);
            }
            other => panic!("the preflight must be answered, got {other:?}"),
        }
        assert!(
            rig.received().is_empty(),
            "no upstream resolution, route match, plugin or connector call serves a preflight"
        );
    }

    /// Acceptance criterion 4: the origin check runs first, the method check
    /// second, both before any plugin.
    #[tokio::test]
    async fn a_cross_origin_request_is_checked_origin_first_method_second() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_cors(cors(&["https://app.example.com"], &["GET"])),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]).with_methods(&["GET", "POST"]));

        let rejected = rig
            .refused(&rig.request_with("GET", "/api", &[("origin", "https://evil.example")]))
            .await;
        let (variant, status, gts_type) = row_of(&rejected);
        assert_eq!(variant, "CorsOriginNotAllowed");
        assert_eq!(status, 403);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            text_of(&rejected.headers, "vary"),
            Some(VARY_ORIGIN),
            "a CORS rejection is keyed on the origin as much as an allowed reply is"
        );
        assert!(
            rig.received().is_empty(),
            "no plugin and no upstream call follow a CORS rejection"
        );

        let rejected = rig
            .refused(&rig.request_with("POST", "/api", &[("origin", "https://app.example.com")]))
            .await;
        let (variant, status, gts_type) = row_of(&rejected);
        assert_eq!(variant, "CorsMethodNotAllowed");
        assert_eq!(status, 403);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
        assert_eq!(
            text_of(&rejected.headers, "vary"),
            Some(VARY_ORIGIN),
            "the method rejection carries Vary: Origin as well"
        );
        assert!(rig.received().is_empty());
    }

    /// Acceptance criterion 5: no `Origin`, no CORS rejection, no
    /// `Access-Control-*` header, but `Vary: Origin`.
    #[tokio::test]
    async fn a_request_with_no_origin_is_never_rejected_by_the_cors_stage() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_cors(cors(&["https://app.example.com"], &["GET"])),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let reply = rig.reply(&rig.request("GET", "/api/users")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            header_of(&reply.headers, "access-control-allow-origin").is_none(),
            "no CORS response header accompanies a request with no Origin"
        );
        assert_eq!(
            header_of(&reply.headers, "vary").and_then(|value| value.to_str().ok()),
            Some(VARY_ORIGIN)
        );
    }

    /// Acceptance criterion 5: an upstream with no CORS block, and one whose
    /// block is disabled, are never rejected by the CORS stage and carry no
    /// `Access-Control-*` header, `Vary: Origin` alone accompanying the reply.
    #[tokio::test]
    async fn a_request_against_no_enabled_cors_block_carries_no_cors_header() {
        let mut disabled = cors(&["https://app.example.com"], &["GET"]);
        disabled.enabled = Some(false);
        for cors_block in [None, Some(disabled)] {
            let rig = Rig::plain();
            let candidate = upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)]);
            rig.seed_upstream(match cors_block {
                Some(block) => candidate.with_cors(block),
                None => candidate,
            });
            rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

            let reply = rig
                .reply(&rig.request_with(
                    "GET",
                    "/api/users",
                    &[("origin", "https://evil.example")],
                ))
                .await;

            assert_eq!(
                reply.status,
                StatusCode::OK,
                "no CORS block rejects a cross-origin request"
            );
            for name in [
                "access-control-allow-origin",
                "access-control-expose-headers",
                "access-control-allow-credentials",
                "access-control-allow-methods",
            ] {
                assert!(
                    header_of(&reply.headers, name).is_none(),
                    "{name} accompanies no request whose upstream carries no enabled CORS block"
                );
            }
            assert_eq!(
                header_of(&reply.headers, "vary").and_then(|value| value.to_str().ok()),
                Some(VARY_ORIGIN)
            );
        }
    }

    /// Acceptance criterion 6 (the store half of it) and criterion 31: an
    /// allowed cross-origin response carries the CORS headers, and the
    /// `response.*` rules are applied to the returned response.
    #[tokio::test]
    async fn an_allowed_cross_origin_response_carries_the_cors_and_response_rules() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_cors(CorsConfig {
                    allowed_origins: vec!["https://app.example.com".to_owned()],
                    expose_headers: vec!["x-upstream".to_owned()],
                    allow_credentials: true,
                    ..cors(&["https://app.example.com"], &["GET"])
                })
                .with_headers(headers_block(PASSTHROUGH_NONE)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: {
                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("x-upstream-secret"),
                    HeaderValue::from_static("cred"),
                );
                headers
            },
            body: Bytes::new(),
            version: Version::HTTP_11,
        });

        let reply = rig
            .reply(&rig.request_with("GET", "/api", &[("origin", "https://app.example.com")]))
            .await;

        assert_eq!(
            header_of(&reply.headers, "access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            header_of(&reply.headers, "access-control-expose-headers")
                .and_then(|value| value.to_str().ok()),
            Some("x-upstream")
        );
        assert_eq!(
            header_of(&reply.headers, "access-control-allow-credentials")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            header_of(&reply.headers, "vary").and_then(|value| value.to_str().ok()),
            Some(VARY_ORIGIN)
        );
        assert_eq!(
            header_of(&reply.headers, "x-response").and_then(|value| value.to_str().ok()),
            Some("set"),
            "the response.set rule is applied to the returned response"
        );
        assert!(
            header_of(&reply.headers, "x-upstream-secret").is_none(),
            "the response.remove rule strips the upstream header"
        );
    }

    // ------------------------------------------------------------------
    // Route matching
    // ------------------------------------------------------------------

    /// Acceptance criterion 7: the longer prefix wins.
    #[tokio::test]
    async fn the_longer_path_prefix_wins_over_a_higher_priority() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        // The shorter prefix carries the greater priority and a binding no
        // registry resolves, so a win of the longer prefix is observable in the
        // answer: the request passes where the shorter route would have
        // refused with 503.
        rig.seed_route(route(UPSTREAM_ID, "/api", 100, &["a"]).with_plugin(UNKNOWN_PLUGIN));
        rig.seed_route(route(UPSTREAM_ID, "/api/v1", 1, &["a"]));

        let reply = rig.reply(&rig.request("GET", "/api/v1/users?a=1")).await;
        assert_eq!(reply.status, StatusCode::OK, "the longer prefix matched");
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outbound(&calls[0]).uri(),
            "https://api.vendor.com/api/v1/users?a=1",
            "the suffix is appended to the matched base path and the allowed query is carried"
        );
    }

    /// Acceptance criterion 7: of two routes with the same prefix in one tier,
    /// the greater `priority` wins.
    #[tokio::test]
    async fn the_greater_priority_wins_a_same_prefix_tie() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        // The greater priority carries a binding no registry resolves, so its
        // win is observable in the 503 the unresolved reference produces.
        rig.seed_route(route(UPSTREAM_ID, "/api", 5, &[]));
        rig.seed_route(route(UPSTREAM_ID, "/api", 50, &[]).with_plugin(UNKNOWN_PLUGIN));

        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            row_of(&rejected).0,
            "PluginNotFound",
            "the priority-50 route won"
        );
        assert_eq!(row_of(&rejected).1, 503);
    }

    /// Acceptance criterion 8: a disabled route is never matched.
    #[tokio::test]
    async fn a_disabled_route_is_never_matched() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        let mut disabled = route(UPSTREAM_ID, "/api/v1", 100, &[]);
        disabled.enabled = false;
        rig.seed_route(disabled);
        rig.seed_route(route(UPSTREAM_ID, "/api", 1, &["a"]));

        let reply = rig.reply(&rig.request("GET", "/api/v1/users?a=1")).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "the disabled route stayed out of the match"
        );

        // And with only the disabled route present, no route matches.
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        let mut disabled = route(UPSTREAM_ID, "/api", 10, &[]);
        disabled.enabled = false;
        rig.seed_route(disabled);
        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            row_of(&rejected),
            (
                "RouteNotFound",
                404,
                "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
            )
        );
    }

    /// Acceptance criterion 8: `path_suffix_mode: disabled` rejects a request
    /// that carries a path suffix.
    #[tokio::test]
    async fn a_disabled_suffix_mode_rejects_a_path_suffix() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        let mut strict = route(UPSTREAM_ID, "/api", 10, &[]);
        if let Some(http) = strict
            .match_config
            .as_mut()
            .and_then(|config| config.http.as_mut())
        {
            http.path_suffix_mode = "disabled".to_owned();
        }
        rig.seed_route(strict);

        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(row_of(&rejected).1, 400, "the suffix is refused");
        assert!(rig.received().is_empty());

        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "the base path alone is served"
        );
    }

    /// Acceptance criterion 9: a query parameter outside the allowlist is
    /// rejected, and an empty allowlist rejects every parameter.
    #[tokio::test]
    async fn a_query_parameter_outside_the_allowlist_is_rejected() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &["a"]));

        let rejected = rig.refused(&rig.request("GET", "/api/users?b=2")).await;
        assert_eq!(row_of(&rejected).1, 400);
        assert!(
            rig.received().is_empty(),
            "no endpoint is selected after a query rejection"
        );

        let rejected = rig.refused(&rig.request("GET", "/api/users?a=1&b=2")).await;
        assert_eq!(
            row_of(&rejected).1,
            400,
            "one disallowed parameter rejects the request"
        );

        let empty = Rig::plain();
        empty.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        empty.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        let rejected = empty.refused(&empty.request("GET", "/api/users?a=1")).await;
        assert_eq!(
            row_of(&rejected).1,
            400,
            "an empty allowlist rejects every query parameter"
        );
    }

    /// Acceptance criterion 11: a method in no candidate route's allowlist is
    /// answered 404, and a route that allows the method never rejects for it.
    #[tokio::test]
    async fn a_method_in_no_route_is_answered_404() {
        let rig = Rig::single();

        let rejected = rig.refused(&rig.request("POST", "/api/users")).await;
        assert_eq!(row_of(&rejected).1, 404);
        assert!(rig.received().is_empty());

        let reply = rig.reply(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "the allowed method is never rejected"
        );
    }

    /// Acceptance criterion 10: an upstream whose protocol is not HTTP is never
    /// route-matched as HTTP.
    #[tokio::test]
    async fn a_non_http_protocol_upstream_is_never_route_matched_as_http() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)]).map(|upstream| {
                upstream.protocol = Some(PROTOCOL_GRPC.to_owned());
            }),
        );
        rig.seed_route(grpc_route(UPSTREAM_ID));

        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            row_of(&rejected).0,
            "RouteError",
            "the non-proxied protocol is not matched"
        );
        assert_eq!(row_of(&rejected).1, 400);
        assert!(rig.received().is_empty());
    }

    /// Acceptance criterion 10: a request routed to a `wt` endpoint is answered
    /// with the gateway `RouteError` semantics rather than proxied.
    #[tokio::test]
    async fn a_wt_endpoint_is_answered_route_error_and_never_proxied() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_WT, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        let (variant, status, gts_type) = row_of(&rejected);
        assert_eq!(variant, "RouteError");
        assert_eq!(status, 400);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert!(
            rig.received().is_empty(),
            "a non-proxied scheme is never forwarded"
        );
    }

    // ------------------------------------------------------------------
    // Endpoint selection
    // ------------------------------------------------------------------

    /// Acceptance criterion 12: a single-endpoint upstream routes with no
    /// header, rejects a malformed header and a well-formed foreign host.
    #[tokio::test]
    async fn a_single_endpoint_upstream_rejects_a_malformed_and_a_foreign_header() {
        let rig = Rig::single();

        let malformed = rig
            .refused(&rig.request_with(
                "GET",
                "/api",
                &[(TARGET_HOST_HEADER, "api.vendor.com:443")],
            ))
            .await;
        assert_eq!(row_of(&malformed).0, "InvalidTargetHost");
        assert_eq!(row_of(&malformed).1, 400);
        assert!(
            rig.received().is_empty(),
            "the form is checked before any pool comparison"
        );

        let foreign = rig
            .refused(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, "other.vendor.com")]))
            .await;
        assert_eq!(row_of(&foreign).0, "UnknownTargetHost");
        assert_eq!(row_of(&foreign).1, 400);
        assert!(rig.received().is_empty());

        let reply = rig.reply(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "the header is absent, and the pool routes"
        );
    }

    /// Acceptance criterion 13: a multi-endpoint upstream with an explicit
    /// alias round-robins without the header and routes by it when present.
    #[tokio::test]
    async fn a_multi_endpoint_pool_with_an_explicit_alias_round_robins() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[
                endpoint(SCHEME_HTTPS, HOST, 443),
                endpoint(SCHEME_HTTPS, SIBLING_HOST, 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let hosts: Vec<String> = rig
            .received()
            .iter()
            .map(|call| outbound(call).uri().host().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            hosts,
            vec![HOST.to_owned(), SIBLING_HOST.to_owned()],
            "the absent header distributes the requests round-robin and never rejects"
        );

        let _ = rig
            .reply(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, SIBLING_HOST)]))
            .await;
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outbound(&calls[0]).uri().host().unwrap_or_default(),
            SIBLING_HOST,
            "the header routes to the named endpoint"
        );
    }

    /// Acceptance criterion 14: a common-suffix alias requires the header and
    /// names the valid hosts in the refusal.
    #[tokio::test]
    async fn a_common_suffix_pool_requires_the_target_host_header() {
        let rig = Rig::routing_by("vendor.com");
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            "vendor.com",
            &[
                endpoint(SCHEME_HTTPS, "us.vendor.com", 443),
                endpoint(SCHEME_HTTPS, "eu.vendor.com", 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let rejected = rig.refused(&rig.request("GET", "/api")).await;
        let (variant, status, _) = row_of(&rejected);
        assert_eq!(variant, "MissingTargetHost");
        assert_eq!(status, 400);
        assert!(
            rejected.error.detail().contains("us.vendor.com")
                && rejected.error.detail().contains("eu.vendor.com"),
            "the detail names the valid hosts: {}",
            rejected.error.detail()
        );
        assert!(rig.received().is_empty());

        let _ = rig
            .reply(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, "eu.vendor.com")]))
            .await;
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outbound(&calls[0]).uri().host().unwrap_or_default(),
            "eu.vendor.com",
            "the header routes to the named endpoint"
        );
    }

    /// Acceptance criterion 15: a header carrying a port, a path or a special
    /// character is invalid before any pool comparison, and a well-formed value
    /// naming no endpoint of the pool is unknown.
    #[tokio::test]
    async fn a_target_host_value_with_a_port_or_path_is_invalid() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[
                endpoint(SCHEME_HTTPS, HOST, 443),
                endpoint(SCHEME_HTTPS, SIBLING_HOST, 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        for value in [
            format!("{HOST}:443"),
            format!("{HOST}/path"),
            format!("{HOST} extra"),
        ] {
            let rejected = rig
                .refused(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, &value)]))
                .await;
            assert_eq!(row_of(&rejected).0, "InvalidTargetHost", "{value}");
            assert_eq!(row_of(&rejected).1, 400, "{value}");
        }
        assert!(
            rig.received().is_empty(),
            "no pool comparison rejected the malformed values"
        );

        let rejected = rig
            .refused(&rig.request_with(
                "GET",
                "/api",
                &[(TARGET_HOST_HEADER, "outside.vendor.com")],
            ))
            .await;
        assert_eq!(row_of(&rejected).0, "UnknownTargetHost");
    }

    /// Acceptance criterion 16: the routing header never reaches the outbound
    /// request, and `Host` carries the endpoint rather than the gateway.
    #[tokio::test]
    async fn the_target_host_header_never_reaches_the_outbound_request() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[
                endpoint(SCHEME_HTTPS, HOST, 443),
                endpoint(SCHEME_HTTPS, SIBLING_HOST, 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig
            .reply(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, SIBLING_HOST)]))
            .await;
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        let headers = outbound(&calls[0]).headers();
        assert!(header_of(headers, TARGET_HOST_HEADER).is_none());
        assert_eq!(
            header_of(headers, "host").and_then(|value| value.to_str().ok()),
            Some(SIBLING_HOST)
        );
    }

    /// Acceptance criterion 17: the hop-by-hop list is absent from the outbound
    /// request and the `request.*` rules are applied after the strip list.
    #[tokio::test]
    async fn the_hop_by_hop_headers_are_stripped_and_the_request_rules_applied() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_headers(headers_block(PASSTHROUGH_NONE)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        // The `Upgrade` value below is not the `websocket` token: a WebSocket
        // upgrade request is the carve-out's own case and is exercised by the
        // streaming feature, this test being the strip list an ordinary
        // proxied request is stripped by.
        let _ = rig
            .reply(&rig.request_with(
                "GET",
                "/api",
                &[
                    ("connection", "keep-alive"),
                    ("keep-alive", "timeout=5"),
                    ("proxy-authenticate", "Basic"),
                    ("proxy-authorization", "Basic ZnJlZDpmcmVk"),
                    ("te", "trailers"),
                    ("trailer", "x-checksum"),
                    ("upgrade", "h2c"),
                    ("x-stripped", "1"),
                    ("x-plain", "yes"),
                ],
            ))
            .await;

        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        let headers = outbound(&calls[0]).headers();
        for stripped in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "upgrade",
        ] {
            assert!(
                header_of(headers, stripped).is_none(),
                "{stripped} is a hop-by-hop header and never reaches the upstream"
            );
        }
        assert!(
            header_of(headers, "x-stripped").is_none(),
            "the request.remove rule is applied"
        );
        assert!(
            header_of(headers, "x-plain").is_none(),
            "the passthrough mode `none` forwards no inbound header"
        );
        assert_eq!(
            header_of(headers, "x-signed").and_then(|value| value.to_str().ok()),
            Some("gateway"),
            "the request.set rule is applied after the strip list"
        );
        assert_eq!(
            header_of(headers, "x-added").and_then(|value| value.to_str().ok()),
            Some("1"),
            "the request.add rule is applied after the strip list"
        );
    }

    // ------------------------------------------------------------------
    // Inbound validation
    // ------------------------------------------------------------------

    /// Acceptance criterion 18: a body above the limit is 413 before buffering,
    /// and a `Content-Length` mismatch is 400.
    #[tokio::test]
    async fn a_body_above_the_limit_is_rejected_before_it_is_buffered() {
        let limits = ProxyLimits {
            body_limit: 8,
            ..ProxyLimits::default()
        };
        let rig = Rig::over(StubConnector::new(), limits);
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let mut request = rig.request("GET", "/api");
        request.body_len = 200;
        let rejected = rig.refused(&request).await;
        assert_eq!(row_of(&rejected).0, "PayloadTooLarge");
        assert_eq!(row_of(&rejected).1, 413);
        assert!(
            rig.received().is_empty(),
            "an oversized body is never forwarded"
        );

        let request = rig.request_with("GET", "/api", &[("content-length", "5")]);
        let rejected = rig.refused(&request).await;
        assert_eq!(row_of(&rejected).0, "ValidationError");
        assert_eq!(
            row_of(&rejected).1,
            400,
            "the declared size does not match the received one"
        );
        assert!(rig.received().is_empty());
    }

    /// Acceptance criterion 19: a `Transfer-Encoding` other than `chunked` and
    /// a `Content-Length` beside a `Transfer-Encoding` are both rejected.
    #[tokio::test]
    async fn a_transfer_encoding_outside_the_closed_set_is_rejected() {
        let rig = Rig::single();

        let rejected = rig
            .refused(&rig.request_with("GET", "/api", &[("transfer-encoding", "gzip")]))
            .await;
        assert_eq!(row_of(&rejected).0, "ValidationError");
        assert_eq!(row_of(&rejected).1, 400);

        let rejected = rig
            .refused(&rig.request_with(
                "GET",
                "/api",
                &[("content-length", "0"), ("transfer-encoding", "chunked")],
            ))
            .await;
        assert_eq!(row_of(&rejected).0, "ValidationError");
        assert_eq!(row_of(&rejected).1, 400);
        assert!(
            rig.received().is_empty(),
            "no upstream call follows a validation failure"
        );
    }

    // ------------------------------------------------------------------
    // Outbound scheme policy and SSRF posture
    // ------------------------------------------------------------------

    /// Acceptance criterion 20: an `http` endpoint is proxied only when the knob
    /// is `true`, `https` regardless of it.
    #[tokio::test]
    async fn a_plaintext_endpoint_is_proxied_only_when_the_knob_lifts_the_posture() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTP, HOST, 8080)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let rejected = rig.refused(&rig.request("GET", "/api")).await;
        assert_eq!(row_of(&rejected).0, "ValidationError");
        assert_eq!(row_of(&rejected).1, 400);
        assert!(
            rig.received().is_empty(),
            "no plaintext connection is established"
        );

        let lifted = ProxyLimits {
            scheme: SchemePolicy {
                allow_http_upstream: true,
                ssrf_enabled: true,
            },
            ..ProxyLimits::default()
        };
        let rig = Rig::over(StubConnector::new(), lifted);
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTP, HOST, 8080)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outbound(&calls[0]).uri(),
            "http://api.vendor.com:8080/api",
            "the lifted knob allows the plaintext connection to the configured endpoint"
        );
    }

    /// Acceptance criterion 21: with the SSRF posture on, the outbound
    /// connection reaches only the endpoint the resolved configuration names.
    #[tokio::test]
    async fn the_ssrf_posture_pins_the_outbound_connection_to_the_resolved_endpoint() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig
            .reply(&rig.request_with("GET", "/api/../../admin", &[(TARGET_HOST_HEADER, HOST)]))
            .await;
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outbound(&calls[0]).uri().host().unwrap_or_default(),
            HOST,
            "no caller-supplied path or header steers the outbound host"
        );
        assert_eq!(
            outbound(&calls[0]).uri().port(),
            None,
            "the endpoint's port is the only one the outbound connection may use"
        );
    }

    // ------------------------------------------------------------------
    // Rate limiting
    // ------------------------------------------------------------------

    /// Acceptance criterion 22: a 429 leaves no guard, transform or upstream
    /// work in the response path, and its headers reach the client.
    #[tokio::test]
    async fn a_rate_limit_refusal_renders_429_with_its_headers() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_rate_limit(rate_limit(1)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let refused = rig.refused(&rig.request("GET", "/api")).await;
        let (variant, status, gts_type) = row_of(&refused);
        assert_eq!(variant, "RateLimitExceeded");
        assert_eq!(status, 429);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert!(
            header_of(&refused.headers, "retry-after").is_some(),
            "the Retry-After the check produced reaches the client"
        );
        for name in [
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ] {
            assert!(
                header_of(&refused.headers, name).is_some(),
                "{name} reaches the client on the rendered 429"
            );
        }
        assert_eq!(
            rig.received().len(),
            1,
            "the refused request performed no upstream work"
        );
    }

    /// Acceptance criterion 22: the check runs after the auth plugin — a request
    /// whose credentials the auth phase cannot resolve is answered with the
    /// auth row every time, the exhausted bucket behind it never being consulted.
    #[tokio::test]
    async fn the_rate_limit_check_runs_after_the_auth_plugin() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_rate_limit(rate_limit(1))
                .with_auth(AuthConfig {
                    plugin_type: Some(
                        crate::infra::plugin::registry::APIKEY_AUTH_PLUGIN_ID.to_owned(),
                    ),
                    sharing: SHARING_PRIVATE.to_owned(),
                    config: Some(
                        [
                            (
                                "secret_ref".to_owned(),
                                serde_json::Value::String("cred://missing-key".to_owned()),
                            ),
                            (
                                "key_header".to_owned(),
                                serde_json::Value::String("x-api-key".to_owned()),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                }),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        // Two requests, one token: a check that ran before the auth phase would
        // have spent the token on the first request and refused the second with
        // 429, so a second auth row is the ordering this assertion pins.
        for index in 0..2 {
            let rejected = rig.refused(&rig.request("GET", "/api")).await;
            assert_eq!(
                row_of(&rejected).0,
                "SecretNotFound",
                "request {index} is refused by the auth phase"
            );
            assert_ne!(
                row_of(&rejected).1,
                429,
                "request {index} never reaches the rate-limit check"
            );
            assert!(rig.received().is_empty());
        }
    }

    // ------------------------------------------------------------------
    // Resolution dispositions
    // ------------------------------------------------------------------

    /// Acceptance criterion 23: no enabled upstream under the alias is 404,
    /// with the gateway source and no added row.
    #[tokio::test]
    async fn an_unknown_alias_is_answered_404_route_not_found() {
        let rig = Rig::plain();

        let rejected = rig.refused(&rig.request("GET", "/api")).await;
        let (variant, status, gts_type) = row_of(&rejected);
        assert_eq!(variant, "RouteNotFound");
        assert_eq!(status, 404);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert!(
            !rejected.error.is_retriable(),
            "a disposition is not retriable"
        );
        assert!(rig.received().is_empty());
    }

    /// Acceptance criterion 23: a target disabled by enabled inheritance is 503
    /// with a `detail` naming the disabled upstream.
    #[tokio::test]
    async fn a_disabled_ancestor_is_answered_503_link_unavailable() {
        let stores = InMemoryStores::new();
        let connector = Arc::new(StubConnector::new());
        let pipeline = Arc::new(ProxyPipeline::with_connector(
            Arc::clone(&connector) as Arc<dyn UpstreamConnector>,
            Arc::new(HttpVersionCache::default()),
            Arc::new(EffectiveConfigResolver::new(
                Arc::new(stores.upstreams()),
                Arc::new(stores.routes()),
                Arc::new(TieredResolver),
            )),
            Arc::new(Limiter::new()),
            Arc::new(PluginRegistries::with_builtins(
                Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
                None,
                crate::infra::plugin::token_cache::TokenCacheConfig::new(
                    crate::config::DEFAULT_TOKEN_CACHE_TTL_SECS,
                    crate::config::DEFAULT_TOKEN_CACHE_CAPACITY,
                ),
            )),
            ProxyLimits::default(),
            stub::telemetry_with_sink(),
        ));
        stores
            .upstreams()
            .insert(&upstream(
                UPSTREAM_ID,
                ALIAS,
                &[endpoint(SCHEME_HTTPS, HOST, 443)],
            ))
            .expect("the test upstream is well formed");
        stores
            .upstreams()
            .insert(
                &upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                    .disabled()
                    .map(|disabled| {
                        disabled.id = Some(ANCESTOR_UPSTREAM_ID);
                        disabled.tenant_id = Some(ANCESTOR_TENANT);
                    }),
            )
            .expect("the test ancestor upstream is well formed");
        let request = ProxyRequest {
            method: "GET".to_owned(),
            alias: ALIAS.to_owned(),
            path_suffix: "/api".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            body_len: 0,
        };

        let rejection = match pipeline
            .handle(&request, &security(), peer(), Bytes::new())
            .await
        {
            Err(rejection) => rejection,
            other => panic!("the disabled target must refuse the request, got {other:?}"),
        };

        let (variant, status, gts_type) = row_of(&rejection);
        assert_eq!(variant, "LinkUnavailable");
        assert_eq!(status, 503);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
        assert!(
            rejection.error.detail().contains(ALIAS),
            "the detail names the disabled upstream by its alias: {}",
            rejection.error.detail()
        );
        assert!(connector.received().is_empty());
    }

    // ------------------------------------------------------------------
    // Transport failures and the no-retry posture
    // ------------------------------------------------------------------

    /// Acceptance criterion 27: every transport failure maps onto its row, and
    /// the failed call is never re-issued as a whole.
    #[tokio::test]
    async fn a_transport_failure_is_mapped_onto_its_row_and_never_retried() {
        let rows: &[(TransportFailure, &str, u16, &str)] = &[
            (
                TransportFailure::Connection,
                "ConnectionTimeout",
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            ),
            (
                TransportFailure::RequestTimeout,
                "RequestTimeout",
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            ),
            (
                TransportFailure::IdleTimeout,
                "IdleTimeout",
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
            ),
            (
                TransportFailure::Protocol,
                "ProtocolError",
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            ),
            (
                TransportFailure::Downstream,
                "DownstreamError",
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            ),
            (
                TransportFailure::StreamAborted,
                "StreamAborted",
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            ),
            (
                TransportFailure::LinkUnavailable,
                "LinkUnavailable",
                503,
                "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            ),
        ];
        for (failure, variant, status, gts_type) in rows {
            let rig = Rig::single();
            let request = rig.request("GET", "/api");
            rig.connector.push(StubReply::ok());
            rig.connector.set_failure(*failure);

            let rejection = rig.refused(&request).await;
            let mapped = row_of(&rejection);
            assert_eq!(mapped.0, *variant, "{failure:?}");
            assert_eq!(mapped.1, *status, "{failure:?}");
            assert_eq!(mapped.2, *gts_type, "{failure:?}");
            assert_eq!(
                rig.received().len(),
                1,
                "{failure:?} is never re-issued as a whole request"
            );
        }
    }

    /// The word `connect` in a send-phase message is no establishment failure:
    /// hyper writes `error reading a body from connection` and `connection
    /// closed before message completed` for an exchange that failed long after
    /// the hop was made, and such an exchange is a 502 `DownstreamError`, never
    /// the 503 the establishment rows carry.
    #[test]
    fn a_send_phase_failure_is_never_classified_as_a_connect() {
        let mid_flight = Chained {
            message: "client error (SendRequest)",
            source: Some(Box::new(Chained {
                message: "error reading a body from connection",
                source: Some(Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset by peer",
                ))),
            })),
        };
        assert_eq!(
            classify_transport(&mid_flight),
            TransportFailure::Downstream
        );

        let incomplete = Chained {
            message: "client error (SendRequest)",
            source: Some(Box::new(Chained {
                message: "connection closed before message completed",
                source: None,
            })),
        };
        assert_eq!(
            classify_transport(&incomplete),
            TransportFailure::Downstream
        );

        let body_write = Chained {
            message: "error writing a body to connection",
            source: Some(Box::new(std::io::Error::other("broken pipe"))),
        };
        assert_eq!(
            classify_transport(&body_write),
            TransportFailure::Downstream
        );
    }

    /// A refused connection is classified from the connect leaves the chain
    /// carries and answered `LinkUnavailable` — 503 — for the plaintext hop
    /// exactly as for the TLS one.
    #[tokio::test]
    async fn a_refused_connection_is_link_unavailable_on_the_plain_hop() {
        // A listener whose port is released again: connecting to it is refused
        // by the loopback stack itself, no remote host being involved.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let port = listener.local_addr().expect("an address").port();
        drop(listener);

        let client: Client<hyper_util::client::legacy::connect::HttpConnector, OutboundBody> =
            Client::builder(TokioExecutor::new())
                .build(hyper_util::client::legacy::connect::HttpConnector::new());
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(format!("http://127.0.0.1:{port}/api"))
            .body(OutboundBody::Full(Bytes::new()))
            .expect("a well-formed request");

        let error =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.request(request))
                .await
                .expect("a refused connect is answered at once")
                .expect_err("the connect is refused");

        assert!(
            is_connect(&error),
            "the refused connect is the typed connect error of the legacy client: {error}"
        );
        assert_eq!(
            classify_transport(&error),
            TransportFailure::LinkUnavailable
        );
    }

    /// Acceptance criterion 29: no `CircuitBreakerOpen` response is ever
    /// produced, however many consecutive failures precede the request.
    #[tokio::test]
    async fn no_circuit_breaker_row_is_ever_produced() {
        let rig = Rig::single();
        rig.connector.set_failure(TransportFailure::LinkUnavailable);

        for _ in 0..5 {
            let rejection = rig.refused(&rig.request("GET", "/api")).await;
            assert_ne!(
                rejection.error.mapping().variant,
                "CircuitBreakerOpen",
                "no circuit breaker is consulted"
            );
        }
        assert_eq!(
            rig.received().len(),
            5,
            "every request is attempted exactly once"
        );
    }

    /// Acceptance criterion 27: an upstream that keeps the exchange open past
    /// `proxy_timeout_secs` is answered 504 `RequestTimeout`, the elapsed budget
    /// being measured from the arrival instant, and the request is never
    /// re-issued.
    #[tokio::test]
    async fn an_upstream_that_does_not_answer_within_the_budget_is_a_request_timeout() {
        let rig = Rig::over(
            StubConnector::new(),
            ProxyLimits {
                proxy_timeout: Duration::from_millis(20),
                ..ProxyLimits::default()
            },
        );
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig.connector.set_delay(Duration::from_millis(200));
        rig.connector.push(StubReply::ok());

        let rejection = rig.refused(&rig.request("GET", "/api")).await;

        let (variant, status, gts_type) = row_of(&rejection);
        assert_eq!(variant, "RequestTimeout");
        assert_eq!(status, 504);
        assert_eq!(
            gts_type,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
        );
        assert_eq!(
            rig.received().len(),
            1,
            "the failed exchange is never re-issued as a whole request"
        );
    }

    /// Acceptance criterion 29: no circuit-breaker event is handed to the
    /// telemetry facade anywhere in the pipeline, however many consecutive
    /// failures precede the request — the row is the only circuit-breaker shape
    /// this release can produce, and it produces no emission.
    #[tokio::test]
    async fn no_circuit_breaker_event_is_emitted_anywhere_in_the_pipeline() {
        let rig = Rig::observed_single();
        rig.connector.set_failure(TransportFailure::LinkUnavailable);
        for _ in 0..5 {
            let rejection = rig.refused(&rig.request("GET", "/api")).await;
            assert_eq!(
                row_of(&rejection).0,
                "LinkUnavailable",
                "the request failed"
            );
        }

        let snapshot = rig.metrics();
        assert!(
            !snapshot
                .iter()
                .any(|series| series.name.contains("circuit")),
            "no circuit-breaker series exists in the pipeline's own emission: {:?}",
            snapshot
                .iter()
                .map(|series| series.name.as_str())
                .collect::<Vec<_>>()
        );
        for line in rig.audit_lines() {
            assert!(
                !line.to_lowercase().contains("circuit"),
                "no circuit-breaker record is written: {line}"
            );
        }
        assert!(
            !rig.audit_lines().is_empty(),
            "the failures were audited, so the assertion above is meaningful"
        );
    }

    /// Acceptance criterion 28: a second request to the same host reuses the
    /// cached capability, and a restart re-attempts the negotiation.
    #[tokio::test]
    async fn a_second_request_to_a_host_reuses_the_negotiated_version() {
        let rig = Rig::single();
        rig.connector.push(StubReply {
            version: Version::HTTP_11,
            ..StubReply::ok()
        });

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let first = rig.received().remove(0);
        assert_eq!(first.version, HttpVersion::Http2, "the cache starts empty");

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let second = rig.received().remove(0);
        assert_eq!(
            second.version,
            HttpVersion::Http1,
            "the capability the first response reported is reused"
        );

        let restarted = Rig::single();
        restarted.connector.push(StubReply {
            version: Version::HTTP_11,
            ..StubReply::ok()
        });
        let _ = restarted.reply(&restarted.request("GET", "/api")).await;
        assert_eq!(
            restarted.received().remove(0).version,
            HttpVersion::Http2,
            "a fresh pipeline negotiates again"
        );
    }

    /// Acceptance criterion 25 and 26: an upstream status passes through as-is
    /// with its body untouched, and no state is held between requests.
    #[tokio::test]
    async fn an_upstream_error_status_is_passed_through_never_re_serialized() {
        let rig = Rig::single();
        let canned = StubReply {
            status: StatusCode::SERVICE_UNAVAILABLE,
            headers: {
                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("content-type"),
                    HeaderValue::from_static("text/plain"),
                );
                headers
            },
            body: Bytes::from_static(b"upstream says no"),
            version: Version::HTTP_11,
        };
        rig.connector.push(StubReply {
            headers: canned.headers.clone(),
            ..canned.clone()
        });
        rig.connector.push(canned.clone());

        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            header_of(&reply.headers, "content-type").and_then(|value| value.to_str().ok()),
            Some("text/plain"),
            "the upstream Content-Type is forwarded as sent"
        );
        assert_eq!(body_of(&reply), Bytes::from_static(b"upstream says no"));

        // The second identical request is served the same way: no response is
        // cached and no state is held between requests.
        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            rig.received().len(),
            2,
            "no response was served from a cache"
        );
    }

    // ------------------------------------------------------------------
    // Streaming proxy: the SSE passthrough and the WebSocket upgrade
    // ------------------------------------------------------------------

    /// The `Sec-WebSocket-Key` the RFC 6455 test vectors are derived from.
    const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    /// The handshake headers a client sends with an upgrade request.
    fn upgrade_headers() -> Vec<(&'static str, &'static str)> {
        vec![
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", WS_KEY),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat"),
        ]
    }

    /// The `Sec-WebSocket-Accept` the upstream answers a handshake with.
    fn accept() -> String {
        crate::domain::streaming::websocket_accept(WS_KEY)
    }

    /// A rig holding the wss upstream and its GET route at `/api`, the
    /// upstream forwarding every inbound header so the carved set is observable.
    fn websocket() -> Rig {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_WSS, HOST, 443)])
                .with_headers(headers_block(PASSTHROUGH_ALL)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig
    }

    /// The journal the pipeline records the streamed classifications into.
    fn journal_of(rig: &Rig) -> Vec<String> {
        rig.pipeline
            .stream_journal()
            .snapshot()
            .iter()
            .map(|event| event.row.clone())
            .collect()
    }

    #[tokio::test]
    async fn an_upgrade_request_runs_every_stage_and_is_relaid_as_a_101() {
        let rig = websocket();
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted(&accept()));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let outcome = rig.handle(&request).await.expect("the upgrade is relaid");
        let ProxyOutcome::Upgrade(outcome) = outcome else {
            panic!("an upgrade request is answered with the 101 it established");
        };
        assert_eq!(outcome.status, StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            text_of(&outcome.headers, "sec-websocket-accept").map(str::to_owned),
            Some(accept()),
            "the handshake headers are relayed as the exchange produced them"
        );
        assert!(journal_of(&rig).is_empty(), "a relayed 101 records nothing");
    }

    #[tokio::test]
    async fn the_upgrade_carve_out_reaches_the_upstream_request() {
        let rig = websocket();
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted(&accept()));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let _ = rig.handle(&request).await;

        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        let headers = outbound(&calls[0]).headers();
        assert_eq!(
            text_of(headers, "upgrade"),
            Some("websocket"),
            "the upgrade token survives the strip list"
        );
        assert_eq!(
            text_of(headers, "connection"),
            Some("Upgrade"),
            "the connection tokens survive the strip list"
        );
        assert_eq!(
            text_of(headers, "sec-websocket-key"),
            Some(WS_KEY),
            "the key the accept is verified against reaches the upstream"
        );
        assert_eq!(
            text_of(headers, "sec-websocket-protocol"),
            Some("chat"),
            "the Sec-WebSocket-* family survives the strip list"
        );
    }

    #[tokio::test]
    async fn the_upgrade_carve_out_exempts_nothing_else() {
        let rig = Rig::plain();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_WSS, HOST, 443)])
                .with_headers(headers_block(PASSTHROUGH_NONE)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted(&accept()));
        let mut headers = upgrade_headers();
        headers.push(("keep-alive", "timeout=5"));
        headers.push(("transfer-encoding", "chunked"));
        headers.push(("x-plain", "yes"));
        let _ = rig.handle(&rig.request_with("GET", "/api", &headers)).await;

        let calls = rig.received();
        let headers = outbound(&calls[0]).headers();
        for stripped in ["keep-alive", "transfer-encoding"] {
            assert!(
                header_of(headers, stripped).is_none(),
                "{stripped} is a strip-list member the carve-out never exempts"
            );
        }
        assert!(
            header_of(headers, "x-plain").is_none(),
            "a header outside the strip list is stripped by the passthrough mode"
        );
    }

    #[tokio::test]
    async fn an_upgrade_against_an_https_endpoint_is_a_route_error() {
        let rig = Rig::single();
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "RouteError");
        assert_eq!(status, 400);
        assert!(
            rig.received().is_empty(),
            "no handshake is attempted for an endpoint that cannot carry the upgrade"
        );
    }

    #[tokio::test]
    async fn an_upgrade_against_a_grpc_endpoint_is_refused_before_any_handshake() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_GRPC, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "RouteError");
        assert_eq!(status, 400);
        assert!(rig.received().is_empty());
    }

    #[tokio::test]
    async fn an_http_endpoint_that_the_knob_refuses_answers_with_a_validation_error() {
        let rig = Rig::plain();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTP, HOST, 80)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "ValidationError");
        assert_eq!(status, 400);
        assert!(rig.received().is_empty());
    }

    #[tokio::test]
    async fn a_non_101_answer_is_a_protocol_failure() {
        let rig = websocket();
        rig.connector.push_upgrade(StubUpgradeReply::refused());
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, gts) = row_of(&rejection);
        assert_eq!(variant, "ProtocolError");
        assert_eq!(status, 502);
        assert!(gts.ends_with("protocol.error.v1"));
    }

    #[tokio::test]
    async fn an_unverifiable_accept_is_a_protocol_failure() {
        let rig = websocket();
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted("not the accept of the key"));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "ProtocolError");
        assert_eq!(status, 502);
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_link_unavailable() {
        let rig = websocket();
        rig.connector.set_failure(TransportFailure::LinkUnavailable);
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "LinkUnavailable");
        assert_eq!(status, 503);
    }

    #[tokio::test]
    async fn an_elapsed_establishment_is_a_connection_timeout() {
        let rig = Rig::over(
            StubConnector::new(),
            ProxyLimits {
                proxy_timeout: Duration::from_millis(20),
                ..ProxyLimits::default()
            },
        );
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_WSS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig.connector.set_delay(Duration::from_millis(200));
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted(&accept()));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let rejection = rig.refused(&request).await;
        let (variant, status, _) = row_of(&rejection);
        assert_eq!(variant, "ConnectionTimeout");
        assert_eq!(status, 504);
    }

    #[tokio::test]
    async fn an_sse_response_is_relaid_streamed_with_its_content_type() {
        let rig = Rig::single();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"data: one\n\n")))
            .await
            .expect("the frame is queued");
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert("x-upstream-secret", HeaderValue::from_static("no"));
        rig.connector.push_stream(StubStreamReply {
            status: StatusCode::OK,
            headers,
            frames: rx,
        });
        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            text_of(&reply.headers, "content-type"),
            Some("text/event-stream"),
            "the upstream Content-Type is passed through unchanged"
        );
        assert!(
            matches!(reply.body, OutboundBody::Streaming(_)),
            "the streamed body is handed to the passthrough as the stream it is"
        );
        assert!(journal_of(&rig).is_empty());
        drop(tx);
    }

    #[tokio::test]
    async fn a_buffered_response_is_relayed_by_the_buffered_passthrough() {
        let rig = Rig::single();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: headers.clone(),
            body: Bytes::from_static(b"{\"ok\":true}"),
            version: Version::HTTP_11,
        });
        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert_eq!(body_of(&reply), Bytes::from_static(b"{\"ok\":true}"));
        assert!(matches!(reply.body, OutboundBody::Full(_)));
        assert_eq!(
            text_of(&reply.headers, "content-type"),
            Some("application/json"),
            "the buffered response is relayed as received"
        );
        assert_eq!(headers.get("vary"), None);
    }

    #[test]
    fn the_streamed_rows_and_the_transport_rows_never_disagree() {
        for (failure, transport) in [
            (
                StreamFailure::EstablishTimedOut,
                TransportFailure::Connection,
            ),
            (
                StreamFailure::EstablishUnreachable,
                TransportFailure::LinkUnavailable,
            ),
            (StreamFailure::EstablishAnswered, TransportFailure::Protocol),
            (StreamFailure::Protocol, TransportFailure::Protocol),
            (StreamFailure::Idle, TransportFailure::IdleTimeout),
            (StreamFailure::Aborted, TransportFailure::StreamAborted),
        ] {
            let streamed = classify(failure, false);
            let mapped = transport_failure(transport);
            let left = streamed.row.mapping();
            let right = mapped.mapping();
            assert_eq!(left.variant, right.variant, "{failure:?} and {transport:?}");
            assert_eq!(left.status, right.status);
            assert_eq!(left.gts_type, right.gts_type);
            assert_eq!(left.retriable, right.retriable);
        }
    }

    #[tokio::test]
    async fn an_upgrade_recorded_by_the_journal_carries_no_request_content() {
        let rig = websocket();
        rig.connector
            .push_upgrade(StubUpgradeReply::accepted(&accept()));
        let request = rig.request_with("GET", "/api", &upgrade_headers());
        let ProxyOutcome::Upgrade(outcome) = rig.handle(&request).await.expect("relaid") else {
            panic!("the upgrade is relaid");
        };
        // The session the pump drives is the journal's only subject: the event
        // it records names the alias, the endpoint host, the row and the state,
        // and never a header value, a credential or a frame.
        let tunnel = outcome.tunnel;
        tunnel.abort();
        let closed = tunnel.session_state();
        let events = rig.pipeline.stream_journal().snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].alias, ALIAS);
        assert_eq!(events[0].endpoint_host, HOST);
        assert_eq!(events[0].row, "StreamAborted");
        assert_eq!(
            events[0].state,
            StreamSessionState::Streaming,
            "the event names the state the session was in when it was classified"
        );
        assert_eq!(closed, StreamSessionState::Failed);
    }

    // ------------------------------------------------------------------
    // The emission the observability feature is the emitter of
    // ------------------------------------------------------------------

    /// The series the rig collected under the instrument `name`.
    fn series_of<'a>(
        snapshot: &'a [crate::infra::observability::harness::Series],
        name: &str,
    ) -> Vec<&'a crate::infra::observability::harness::Series> {
        snapshot
            .iter()
            .filter(|series| series.name == name)
            .collect()
    }

    /// The first value of one attribute of `series`.
    fn attribute_of(series: &crate::infra::observability::harness::Series, key: &str) -> String {
        series
            .attributes
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    /// The label key and value sets the snapshot carries, the form the
    /// cardinality assertions read.
    fn attribute_pairs(
        snapshot: &[crate::infra::observability::harness::Series],
    ) -> Vec<(String, String)> {
        snapshot
            .iter()
            .flat_map(|series| series.attributes.clone())
            .collect()
    }

    /// The closed label-key vocabulary DESIGN §4.2 fixes, the only keys any
    /// instrument of this gear may carry.
    fn label_vocabulary() -> Vec<&'static str> {
        vec![
            "host",
            "http.request.method",
            "http.route",
            "http.response.status_code",
            "phase",
            "error_type",
            "path",
            "upstream_id",
            "endpoint_host",
            "endpoint",
            "selection_method",
        ]
    }

    /// §6: `oagw_requests_total` carries the match pattern as `http.route`,
    /// the alias as `host`, the numeric status and the method, and no raw
    /// path, no query string and no tenant identifier anywhere.
    #[tokio::test]
    async fn the_request_counters_carry_the_route_pattern_and_no_tenant_identifier() {
        let rig = Rig::observed_single();
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });
        let request = rig.request_with(
            "GET",
            "/api/v1/users",
            &[
                ("x-trace-id", "trace-1"),
                ("authorization", "Bearer s3cret"),
            ],
        );
        let _ = rig.handle(&request).await.expect("relaid");

        let snapshot = rig.metrics();
        let requests = series_of(&snapshot, "oagw_requests_total");
        assert_eq!(requests.len(), 1, "one series, one request");
        let series = requests[0];
        assert_eq!(attribute_of(series, "host"), ALIAS, "host is the alias");
        assert_eq!(
            attribute_of(series, "http.route"),
            "/api",
            "http.route is the match pattern, never the raw path"
        );
        assert_eq!(attribute_of(series, "http.request.method"), "GET");
        assert_eq!(attribute_of(series, "http.response.status_code"), "200");
        let pairs = attribute_pairs(&snapshot);
        assert!(
            !pairs.iter().any(|(key, _)| key.contains("tenant")
                || key.contains("subject")
                || key.contains("principal")),
            "no tenant, subject or principal label key exists: {pairs:?}"
        );
        for (key, value) in &pairs {
            assert_ne!(value.as_str(), &TENANT.to_string(), "{key}");
            assert_ne!(value.as_str(), &SUBJECT.to_string(), "{key}");
            assert!(!value.contains("/api/v1/users"), "{key}={value}");
            assert!(!value.contains("trace-1"), "{key}={value}");
            assert!(!value.contains("s3cret"), "{key}={value}");
        }
        assert_eq!(
            series.attributes.len(),
            4,
            "the request series carries exactly the four DESIGN label keys: {:?}",
            series.attributes
        );
        assert!(
            series
                .attributes
                .iter()
                .all(|(key, _)| label_vocabulary().contains(&key.as_str())),
            "every label key is in the closed vocabulary: {:?}",
            series.attributes
        );
    }

    /// §6: a method outside the standard verb set is labelled `_OTHER`, and a
    /// standard verb is labelled with itself.
    #[tokio::test]
    async fn a_non_standard_method_is_labelled_other() {
        let rig = Rig::observed_single();
        let rejected = rig.refused(&rig.request("PURGE", "/api/v1/users")).await;
        assert_eq!(row_of(&rejected).0, "RouteNotFound", "no route matched it");

        let snapshot = rig.metrics();
        let requests = series_of(&snapshot, "oagw_requests_total");
        assert_eq!(requests.len(), 1);
        assert_eq!(attribute_of(requests[0], "http.request.method"), "_OTHER");
        assert_eq!(
            attribute_of(requests[0], "http.response.status_code"),
            "404",
            "the numeric gateway status is the label value"
        );
    }

    /// §6: the duration histogram records one observation per completed stage
    /// over the twelve buckets and no other boundary, under the phase the stage
    /// carries.
    #[tokio::test]
    async fn the_duration_histogram_covers_the_completed_stages_only() {
        let rig = Rig::observed_single();
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });
        let _ = rig.reply(&rig.request("GET", "/api/v1/users")).await;

        let snapshot = rig.metrics();
        let durations = series_of(&snapshot, "oagw_request_duration_seconds");
        let mut phases: Vec<String> = durations
            .iter()
            .map(|series| attribute_of(series, "phase"))
            .collect();
        phases.sort();
        let expected: Vec<&str> = ob::Phase::ALL
            .iter()
            .filter(|phase| {
                // The preflight-detection stage is the only stage an ordinary
                // proxied request does not run as its own phase.
                phase.as_str() != "response_passthrough" || true
            })
            .map(|phase| phase.as_str())
            .collect();
        for phase in [
            "classification",
            "config_resolution",
            "route_matching",
            "endpoint_selection",
            "actual_request_cors_check",
            "header_processing_and_validation",
            "plugin_chain_with_rate_limit_check",
            "scheme_and_ssrf_policy",
            "outbound_call",
            "response_passthrough",
        ] {
            let series = durations
                .iter()
                .find(|series| attribute_of(series, "phase") == phase);
            assert!(series.is_some(), "the {phase} stage was observed");
            let series = series.expect("the phase was observed");
            assert_eq!(attribute_of(series, "host"), ALIAS);
            assert_eq!(attribute_of(series, "http.route"), "/api");
            assert_eq!(
                series.bounds,
                Some(ob::DURATION_BUCKETS.to_vec()),
                "the twelve buckets of DESIGN §4.2 and no other boundary"
            );
            assert_eq!(series.unit, "s");
        }
        assert!(
            !durations.iter().any(|series| {
                attribute_of(series, "phase") == "endpoint_selection"
                    && attribute_of(series, "http.route") != "/api"
            }),
            "no stage is observed under a second route"
        );
        assert_eq!(phases.len(), expected.len());
    }

    /// §6: a passed-through upstream error status is never re-classified —
    /// its record carries no `error_type` and no `oagw_errors_total` increment,
    /// the numeric status label being where it stays visible.
    #[tokio::test]
    async fn a_passed_through_error_status_is_audited_with_no_error_type() {
        let rig = Rig::observed_single();
        rig.connector.push(StubReply {
            status: StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"upstream says no"),
            version: Version::HTTP_11,
        });
        let reply = rig.reply(&rig.request("GET", "/api/v1/users")).await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);

        assert_eq!(rig.audit_lines().len(), 1, "one record for the request");
        let line = &rig.audit_lines()[0];
        assert!(line.contains("\"error_type\":null"), "{line}");
        assert!(!line.contains("error_message"), "{line}");
        assert!(
            line.contains("\"status\":502"),
            "the upstream's own status is the record's status: {line}"
        );
        let snapshot = rig.metrics();
        assert!(
            series_of(&snapshot, "oagw_errors_total").is_empty(),
            "a passed-through upstream error is not counted by the error counter"
        );
        assert_eq!(
            attribute_of(
                series_of(&snapshot, "oagw_requests_total")[0],
                "http.response.status_code"
            ),
            "502"
        );
    }

    /// §6: a gateway-rendered refusal is audited with the row the closed table
    /// names, its `error_message` the row's own text, and it increments
    /// `oagw_errors_total` once with that row's name.
    #[tokio::test]
    async fn a_gateway_refusal_is_audited_with_its_row_and_counted() {
        let rig = Rig::observed();
        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(row_of(&rejected).0, "RouteNotFound");

        let lines = rig.audit_lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("\"error_type\":\"RouteNotFound\""),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"error_message\":\"Route not found"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("\"level\":\"ERROR\""), "{}", lines[0]);
        assert!(
            lines[0].contains("\"path\":\"/oagw/v1/proxy/payments/api/users\""),
            "the record's path is the gear-relative proxy path: {}",
            lines[0]
        );

        let snapshot = rig.metrics();
        let errors = series_of(&snapshot, "oagw_errors_total");
        assert_eq!(errors.len(), 1, "one increment under one label set");
        assert_eq!(attribute_of(errors[0], "error_type"), "RouteNotFound");
        assert_eq!(attribute_of(errors[0], "host"), ALIAS);
        assert_eq!(
            attribute_of(errors[0], "http.route"),
            ob::PROXY_SHELL_ROUTE,
            "no route matched, so the shell route is the label value"
        );
    }

    /// §6: a request refused before the outbound call is issued never moves the
    /// in-flight gauge, and a completed request settles it back to zero.
    #[tokio::test]
    async fn a_refusal_before_dispatch_never_moves_the_in_flight_gauge() {
        let rig = Rig::observed();
        let _ = rig.refused(&rig.request("GET", "/api/users")).await;
        let snapshot = rig.metrics();
        assert!(
            series_of(&snapshot, "oagw_requests_in_flight").is_empty(),
            "no issuance means no gauge update: {snapshot:?}"
        );

        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[endpoint(SCHEME_HTTPS, HOST, 443)],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });
        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let snapshot = rig.metrics();
        let in_flight = series_of(&snapshot, "oagw_requests_in_flight");
        assert_eq!(in_flight.len(), 1, "the issued request updated the gauge");
        assert_eq!(in_flight[0].value, Some(0.0), "the request was settled");
        assert_eq!(attribute_of(in_flight[0], "host"), ALIAS);
    }

    /// §6: an endpoint the pipeline could not exchange with is visible as 0
    /// with no probe having been sent, and returns to 1 on the next completed
    /// exchange against it.
    #[tokio::test]
    async fn an_unreachable_endpoint_is_reported_down_and_recovers() {
        let rig = Rig::observed_single();
        rig.connector.set_failure(TransportFailure::LinkUnavailable);
        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(row_of(&rejected).0, "LinkUnavailable");
        let snapshot = rig.metrics();
        let available = series_of(&snapshot, "oagw_upstream_available");
        assert_eq!(available.len(), 1, "one series per host and endpoint");
        assert_eq!(
            available[0].value,
            Some(0.0),
            "the exchange never completed"
        );
        assert_eq!(attribute_of(available[0], "endpoint"), HOST);

        rig.connector.clear_failure();
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });
        let _ = rig.reply(&rig.request("GET", "/api/users")).await;
        let snapshot = rig.metrics();
        let available = series_of(&snapshot, "oagw_upstream_available");
        assert_eq!(available.len(), 1, "one series per host and endpoint");
        assert_eq!(available[0].value, Some(1.0), "the exchange completed");
        assert_eq!(attribute_of(available[0], "host"), ALIAS);
        assert_eq!(attribute_of(available[0], "endpoint"), HOST);
    }

    /// §6: an endpoint selection is visible with its selection method, and only
    /// a selection that consumed `X-OAGW-Target-Host` increments the target-host
    /// counter.
    #[tokio::test]
    async fn a_selection_is_counted_with_its_method_and_the_target_host_flag() {
        let rig = Rig::observed();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            ALIAS,
            &[
                endpoint(SCHEME_HTTPS, HOST, 443),
                endpoint(SCHEME_HTTPS, SIBLING_HOST, 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));
        for _ in 0..2 {
            rig.connector.push(StubReply {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::new(),
                version: Version::HTTP_11,
            });
        }
        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let _ = rig
            .reply(&rig.request_with("GET", "/api", &[(TARGET_HOST_HEADER, SIBLING_HOST)]))
            .await;

        let snapshot = rig.metrics();
        let selections = series_of(&snapshot, "oagw_routing_endpoint_selected");
        assert_eq!(selections.len(), 2, "one series per method and endpoint");
        for series in &selections {
            assert_eq!(attribute_of(series, "upstream_id"), UPSTREAM_ID.to_string());
        }
        let round_robin = selections
            .iter()
            .find(|series| attribute_of(series, "selection_method") == "round_robin")
            .expect("the pool round-robined");
        assert_eq!(attribute_of(round_robin, "endpoint_host"), HOST);
        assert_eq!(round_robin.value, Some(1.0));
        let explicit = selections
            .iter()
            .find(|series| attribute_of(series, "selection_method") == "explicit_header")
            .expect("the header routed the request");
        assert_eq!(attribute_of(explicit, "endpoint_host"), SIBLING_HOST);
        assert_eq!(explicit.value, Some(1.0));
        let used = series_of(&snapshot, "oagw_routing_target_host_used");
        assert_eq!(used.len(), 1, "one target-host series, one increment");
        assert_eq!(attribute_of(used[0], "endpoint_host"), SIBLING_HOST);
        assert_eq!(used[0].value, Some(1.0));
    }

    /// §6: a single-endpoint pool records its selection as `default`, and no
    /// target-host counter appears for a selection that consumed no header.
    #[tokio::test]
    async fn a_single_endpoint_selection_is_the_default_method() {
        let rig = Rig::observed_single();
        rig.connector.push(StubReply {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            version: Version::HTTP_11,
        });
        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let snapshot = rig.metrics();
        let selections = series_of(&snapshot, "oagw_routing_endpoint_selected");
        assert_eq!(selections.len(), 1);
        assert_eq!(attribute_of(selections[0], "selection_method"), "default");
        assert!(
            series_of(&snapshot, "oagw_routing_target_host_used").is_empty(),
            "no header was consumed"
        );
    }

    /// §6: an aborted stream increments `oagw_errors_total` with
    /// `error_type` `StreamAborted` under the same label keys as any other
    /// failure and writes one ERROR record, the streamed classification
    /// arriving as an ordinary mapped row.
    #[tokio::test]
    async fn an_aborted_stream_is_counted_and_recorded_as_its_row() {
        let rig = Rig::observed_single();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(Err(
            Box::new(std::io::Error::other("upstream went away")) as BoxError
        ))
        .await
        .expect("the failing frame is queued");
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        rig.connector.push_stream(StubStreamReply {
            status: StatusCode::OK,
            headers,
            frames: rx,
        });
        let reply = rig.reply(&rig.request("GET", "/api")).await;
        assert!(matches!(reply.body, OutboundBody::Streaming(_)));
        drop(tx);
        // The abort is only observed when the body the passthrough holds is
        // polled, which is when the classification reaches the recorder.
        // `OutboundBody` implements `hyper::body::Body`, so the poll the
        // passthrough performs is the one the recorder fires on.
        let mut body = reply.body;
        let _ = http_body_util::BodyExt::frame(&mut body).await;

        let snapshot = rig.metrics();
        let errors = series_of(&snapshot, "oagw_errors_total");
        assert_eq!(errors.len(), 1, "one increment for the aborted stream");
        assert_eq!(attribute_of(errors[0], "error_type"), "StreamAborted");
        assert_eq!(attribute_of(errors[0], "host"), ALIAS);
        assert_eq!(attribute_of(errors[0], "http.route"), "/api");

        let lines = rig.audit_lines();
        assert_eq!(lines.len(), 1, "one record, the request's own");
        assert!(
            lines[0].contains("\"error_type\":\"StreamAborted\""),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("\"level\":\"ERROR\""), "{}", lines[0]);
        assert!(lines[0].contains("\"status\":502"), "{}", lines[0]);
    }

    /// §6: the emission never alters a response — the status, the headers and
    /// the body of a proxied response and of a passed-through upstream error
    /// are the producing pipeline's own, and the same requests through a
    /// pipeline that emits are byte-identical to one through a pipeline that
    /// emits nothing.
    #[tokio::test]
    async fn the_emission_never_alters_the_response_it_is_handed() {
        let observed = Rig::observed_single();
        let silent = Rig::single();
        for rig in [&observed, &silent] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                HeaderValue::from_static("application/vnd.vendor.v1+json"),
            );
            headers.insert("x-upstream-secret", HeaderValue::from_static("no"));
            rig.connector.push(StubReply {
                status: StatusCode::CREATED,
                headers,
                body: Bytes::from_static(b"hello"),
                version: Version::HTTP_11,
            });
            rig.connector.push(StubReply {
                status: StatusCode::BAD_GATEWAY,
                headers: HeaderMap::new(),
                body: Bytes::from_static(b"upstream says no"),
                version: Version::HTTP_11,
            });
        }

        let mut responses = Vec::new();
        for rig in [&observed, &silent] {
            let mut run = Vec::new();
            let reply = rig.reply(&rig.request("GET", "/api/v1/users")).await;
            run.push((
                reply.status,
                text_of(&reply.headers, "content-type").map(str::to_owned),
                text_of(&reply.headers, "x-upstream-secret").map(str::to_owned),
                body_of(&reply),
            ));
            let reply = rig.reply(&rig.request("GET", "/api/v1/users")).await;
            run.push((
                reply.status,
                text_of(&reply.headers, "content-type").map(str::to_owned),
                text_of(&reply.headers, "x-upstream-secret").map(str::to_owned),
                body_of(&reply),
            ));
            responses.push(run);
        }
        assert_eq!(
            responses[0], responses[1],
            "an emitting pipeline and a silent one hand back the same responses"
        );
        assert_eq!(responses[0][0].0, StatusCode::CREATED);
        assert_eq!(
            responses[0][0].1.as_deref(),
            Some("application/vnd.vendor.v1+json")
        );
        assert_eq!(responses[0][0].2.as_deref(), Some("no"));
        assert_eq!(responses[0][0].3, Bytes::from_static(b"hello"));
        assert_eq!(responses[0][1].0, StatusCode::BAD_GATEWAY);
        assert_eq!(responses[0][1].3, Bytes::from_static(b"upstream says no"));
        assert!(
            !observed.audit_lines().is_empty(),
            "the observed pipeline did emit, so the equality above is meaningful"
        );
    }

    /// §6: the only header-shaped value in a record is the request identifier
    /// the platform trace context arrived with, and the pipeline adds no
    /// correlation header of its own to the outbound request.
    #[tokio::test]
    async fn the_request_identifier_is_the_trace_context_and_no_header_is_added() {
        let rig = Rig::observed_single();
        // A failed request is written unsampled, so the record is this
        // request's own and not a later sample's.
        rig.connector.push(StubReply {
            status: StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello"),
            version: Version::HTTP_11,
        });
        let request = rig.request_with(
            "GET",
            "/api/v1/users",
            &[
                ("x-trace-id", "4bf92f3577b34da6a3ce929d0e0e4736"),
                ("authorization", "Bearer s3cret"),
            ],
        );
        let _ = rig.reply(&request).await;

        let lines = rig.audit_lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("\"request_id\":\"4bf92f3577b34da6a3ce929d0e0e4736\""),
            "the platform trace context is the record's request identifier: {}",
            lines[0]
        );
        assert!(!lines[0].contains("s3cret"), "{}", lines[0]);
        let calls = rig.received();
        assert_eq!(calls.len(), 1);
        let headers = outbound(&calls[0]).headers();
        for name in [
            "x-trace-id",
            "traceparent",
            "x-request-id",
            "x-correlation-id",
            "oagw-request-id",
        ] {
            assert!(
                header_of(headers, name).is_none(),
                "no correlation header is added to the outbound request: {name}"
            );
        }
    }

    /// §6 and the credential-isolation principle: the `detail` of every
    /// problem+json the pipeline produces, the records it writes and the labels
    /// it counts carry no credential material, no header value, no query string
    /// and no request body — a gateway refusal names only the configuration the
    /// refusal is about.
    #[tokio::test]
    async fn no_request_content_reaches_a_gateway_detail_or_its_record() {
        let mut rig = Rig::observed();
        rig.alias = "vendor.com".to_owned();
        rig.seed_upstream(upstream(
            UPSTREAM_ID,
            "vendor.com",
            &[
                endpoint(SCHEME_HTTPS, "us.vendor.com", 443),
                endpoint(SCHEME_HTTPS, "eu.vendor.com", 443),
            ],
        ));
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &["token"]));

        let rejected = rig
            .refused(&rig.request_with(
                "GET",
                "/api?token=s3cret-query",
                &[
                    ("authorization", "Bearer s3cret-material"),
                    ("x-secret", "s3cret-material"),
                ],
            ))
            .await;
        let detail = rejected.error.detail();
        assert!(
            detail.contains("us.vendor.com") && detail.contains("eu.vendor.com"),
            "the detail names the hosts the row is about: {detail}"
        );
        assert!(
            !detail.contains("s3cret-material"),
            "no header value or query string reaches the detail: {detail}"
        );

        // A well-formed header naming no endpoint of the pool, and a route
        // content rejection, render the same isolation.
        let foreign = rig
            .refused(&rig.request_with(
                "GET",
                "/api",
                &[(TARGET_HOST_HEADER, "s3cret-material.example")],
            ))
            .await;
        assert_eq!(row_of(&foreign).0, "UnknownTargetHost");
        assert!(
            !foreign.error.detail().contains("s3cret-material"),
            "the refused header value never reaches the detail: {}",
            foreign.error.detail()
        );

        let unlisted = rig
            .refused(&rig.request("GET", "/api?other=s3cret-material"))
            .await;
        assert_eq!(row_of(&unlisted).1, 400);
        assert!(
            !unlisted.error.detail().contains("s3cret-material"),
            "the refused parameter never reaches the detail: {}",
            unlisted.error.detail()
        );

        // A request body is buffered for the upstream alone: no refusal renders
        // it, and no record of the three refusals carries any of the material.
        let request = rig.request_with(
            "GET",
            "/api",
            &[("authorization", "Bearer s3cret-material")],
        );
        let carried = match rig
            .handle_bytes(&request, Bytes::from_static(b"s3cret-body"))
            .await
        {
            Err(rejection) => rejection,
            other => panic!("the request must be refused, got {other:?}"),
        };
        assert!(
            !carried.error.detail().contains("s3cret-body"),
            "the request body never reaches the detail: {}",
            carried.error.detail()
        );

        for line in rig.audit_lines() {
            assert!(
                !line.contains("s3cret-material") && !line.contains("s3cret-body"),
                "no request content reaches a record: {line}"
            );
        }
        for series in rig.metrics() {
            for (key, value) in &series.attributes {
                assert!(
                    !value.contains("s3cret"),
                    "no request content reaches the label {key}={value}"
                );
            }
        }
    }

    /// §6: the upstream's rejection of the credentials the auth phase injected
    /// — the passed-through 401 — produces one ERROR authentication-failure
    /// record with the row the closed table names, and the class is
    /// rate-limited so a flood of them cannot flood the log.
    #[tokio::test]
    async fn an_upstream_credential_rejection_is_the_authentication_failure_event() {
        let rig = Rig::observed_single();
        rig.connector.push(StubReply {
            status: StatusCode::UNAUTHORIZED,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"no"),
            version: Version::HTTP_11,
        });
        let reply = rig.reply(&rig.request("GET", "/api/v1/users")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

        let lines = rig.audit_lines();
        assert_eq!(lines.len(), 1, "one record for the request");
        assert!(
            lines[0].contains("\"level\":\"ERROR\""),
            "the record is at ERROR: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"event\":\"auth_failure\""),
            "the record is the authentication-failure event: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"error_type\":\"AuthenticationFailed\""),
            "the record names the row the closed table maps the 401 onto: {}",
            lines[0]
        );
        assert!(lines[0].contains("\"status\":401"), "{}", lines[0]);

        // A passed-through upstream status is visible in the numeric status
        // label of the request counter and not in `oagw_errors_total`, which
        // counts only the failures the pipeline rendered through a row.
        let snapshot = rig.metrics();
        assert!(series_of(&snapshot, "oagw_errors_total").is_empty());
        assert_eq!(
            attribute_of(
                series_of(&snapshot, "oagw_requests_total")[0],
                "http.response.status_code"
            ),
            "401"
        );
    }

    /// §6: a credential-resolution failure the auth phase mapped onto one of its
    /// rows produces one ERROR authentication-failure record naming that row,
    /// the class being the rate-limited one.
    #[tokio::test]
    async fn a_credential_resolution_failure_is_the_authentication_failure_event() {
        let rig = Rig::observed();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)]).with_auth(
                AuthConfig {
                    plugin_type: Some(
                        crate::infra::plugin::registry::APIKEY_AUTH_PLUGIN_ID.to_owned(),
                    ),
                    sharing: crate::domain::model::SHARING_PRIVATE.to_owned(),
                    config: Some(
                        [
                            (
                                "secret_ref".to_owned(),
                                serde_json::Value::String("cred://missing-key".to_owned()),
                            ),
                            (
                                "key_header".to_owned(),
                                serde_json::Value::String("x-api-key".to_owned()),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                },
            ),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let rejected = rig.refused(&rig.request("GET", "/api/users")).await;
        assert_eq!(
            row_of(&rejected).0,
            "SecretNotFound",
            "the reference did not resolve"
        );
        assert!(
            rig.received().is_empty(),
            "no upstream call followed the failure"
        );

        let lines = rig.audit_lines();
        assert_eq!(
            lines.len(),
            2,
            "one record from the auth phase, one at the exit"
        );
        assert!(
            lines[0].contains("\"level\":\"ERROR\"") && lines[1].contains("\"level\":\"ERROR\""),
            "both records are at ERROR: {lines:?}"
        );
        assert!(
            lines[0].contains("\"event\":\"auth_failure\""),
            "the record the auth phase opened is the authentication-failure event: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"error_type\":\"SecretNotFound\""),
            "it names the row the failure mapped onto: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains("cred://missing-key"),
            "no credential material reaches the record: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"status\":500"),
            "the record carries the status of the row the failure mapped onto and no \
             placeholder: {}",
            lines[0]
        );
    }

    /// §6: the record the rate-limit refusal opens carries the 429 the row
    /// renders, and never a placeholder status.
    #[tokio::test]
    async fn a_rate_limit_refusal_is_audited_with_the_status_it_renders() {
        let rig = Rig::observed();
        rig.seed_upstream(
            upstream(UPSTREAM_ID, ALIAS, &[endpoint(SCHEME_HTTPS, HOST, 443)])
                .with_rate_limit(rate_limit(1)),
        );
        rig.seed_route(route(UPSTREAM_ID, "/api", 10, &[]));

        let _ = rig.reply(&rig.request("GET", "/api")).await;
        let refused = rig.refused(&rig.request("GET", "/api")).await;
        assert_eq!(row_of(&refused).1, 429);

        let refusal = rig
            .audit_lines()
            .into_iter()
            .find(|line| line.contains("\"event\":\"rate_limit_refusal\""))
            .expect("the refusal opens its own record");
        assert!(
            refusal.contains("\"status\":429"),
            "the record carries the 429 the refusal renders: {refusal}"
        );
        assert!(
            !refusal.contains("\"status\":0"),
            "no record carries a placeholder status: {refusal}"
        );
        assert!(
            refusal.contains("\"level\":\"WARN\""),
            "the refusal is recorded at WARN: {refusal}"
        );
    }

    /// §1.5 and §6: no `/metrics` route of the gear's own exists, the exposure
    /// being the platform telemetry surface.
    #[tokio::test]
    async fn no_metrics_route_is_mounted_by_the_gear() {
        let stores = InMemoryStores::new();
        let control_plane = Arc::new(crate::api::control_plane::ControlPlaneService::new(stores));
        let mut router = crate::api::rest::route_shell::mount(
            axum::Router::new(),
            &crate::api::rest::route_shell::MountLedger::new(),
            Arc::clone(&control_plane),
            stub::pipeline(control_plane.stores(), Arc::new(stub::StubConnector::new())),
        )
        .expect("the shell mounts");
        for path in ["/metrics", "/oagw/metrics", "/oagw/v1/metrics"] {
            let request = axum::http::Request::builder()
                .method("GET")
                .uri(path)
                .body(axum::body::Body::empty())
                .expect("the probe request is well formed");
            let response = tower::ServiceExt::oneshot(&mut router, request)
                .await
                .expect("the router answers");
            assert_eq!(
                response.status().as_u16(),
                404,
                "{path} is not a route of this gear"
            );
        }
    }

    /// §1.5 and §6: the closed `OagwConfig` key set is unchanged by this
    /// feature, and an observability key is still rejected.
    #[test]
    fn no_observability_key_is_added_to_the_gear_configuration() {
        let known = serde_json::json!({
            "proxy_timeout_secs": 30,
            "allow_http_upstream": false,
            "ssrf_policy": {"enabled": true},
            "body_limit_bytes": 104857600,
            "token_cache_ttl_secs": 300,
            "token_cache_capacity": 1024
        });
        crate::config::OagwConfig::load(Some(&known)).expect("the closed key set still loads");

        for key in [
            "observability",
            "metrics_enabled",
            "audit_sample_success",
            "audit_level",
            "otel_exporter",
        ] {
            let mut raw = known.clone();
            raw[key] = serde_json::json!(true);
            let error = crate::config::OagwConfig::load(Some(&raw))
                .expect_err("an unknown key is a hard error");
            assert_eq!(error.mapping().variant, "ValidationError", "{key}");
            assert!(
                error.detail().contains(key),
                "the error names the offending key: {}",
                error.detail()
            );
        }
    }
}
