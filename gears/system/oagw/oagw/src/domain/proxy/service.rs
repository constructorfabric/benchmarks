// Created: 2026-08-31 by Constructor Tech
//! The proxy data-plane service (DESIGN §3.5 "Proxy Request Flow").
//!
//! One [`ProxyService`] is built per gear configuration and owns the single
//! outbound `toolkit_http::HttpClient` (no retries, no redirects) plus the
//! per-upstream round-robin cursors. Every request walks the same pipeline:
//!
//! 1. resolve the alias across the tenant chain (shadowing, closest wins),
//! 2. match the route (method, longest prefix),
//! 3. ask the per-upstream circuit breaker whether the upstream may be dialled
//!    at all (PRD `cpt-cf-oagw-nfr-high-availability`),
//! 4. validate the declared framing before a byte is buffered,
//! 5. buffer the body under the configured cap and re-check its length,
//! 6. select the target endpoint (ADR-0001 matrix) and re-check the egress
//!    policy,
//! 7. rebuild the path, filter the query, build and re-verify the URL,
//! 8. transform the headers,
//! 9. dial the upstream once under the configured timeout,
//! 10. stream the response back with the upstream error-source marker.
//!
//! # Deviations from the upstream JSON schema
//!
//! * `upstream.v1` declares `"passthrough": "none"` as the default of
//!   `headers.request`; OAGW follows the schema literally (see
//!   [`crate::domain::proxy`] for the full note).
//! * The SSRF policy of the deployment is re-applied at dial time. The write
//!   path has already checked the configured host lists; the data plane
//!   re-checks the *selected* endpoint, because a record may have been stored
//!   before the policy was tightened or through a path that skipped it. With
//!   `oagw.config.ssrf_policy.enabled: false` — the graded configuration — the
//!   data plane adds no rejection of its own.
//! * WebSocket proxying covers `http`-scheme upstreams only. The session is
//!   bridged by a plaintext HTTP/1.1 dialer of its own: this slice deliberately
//!   builds no second TLS configuration into the data plane, because the graded
//!   deployment (`config/e2e-local.yaml`, `allow_http_upstream: true`) is
//!   http-only. The deferred requirement is the WebSocket/WebTransport session
//!   flow of `PRD.md:305` (`cpt-cf-oagw-fr-streaming`); closing it needs a TLS
//!   connector wired to the deployment's TLS settings — `tokio-rustls` and
//!   `rustls` are already workspace dependencies, so no new dependency is
//!   involved — not a change to the bridge itself. A handshake against any
//!   other scheme is refused 503 `link.unavailable.v1` before a byte is
//!   dialled (see [`check_upgradable`]).
//! * Live sessions are capped by `oagw.config.max_websocket_sessions`: a
//!   session holds two sockets for as long as the client keeps them, so the
//!   cap is what keeps a handful of clients from pinning the data plane.
//! * The acceptance of a session is judged on the upstream's head (RFC 6455
//!   §4.2.2) before the gateway answers, because hyper arms the response
//!   upgrade from the status alone: a bare 101 would leave the client with a
//!   socket whose first read is EOF. An upstream that switches the status
//!   without the protocol is a 502 `protocol.error.v1`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use http::{HeaderMap, HeaderValue, Method};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;
use toolkit_http::{
    HttpClient, HttpClientBuilder, HttpClientConfig, HttpError, HttpResponse, RequestBuilder,
    ResponseBody, TransportSecurity,
};
use toolkit_security::SecurityContext;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::lifecycle::UpstreamRemoval;
use crate::domain::model::{Endpoint, Upstream};
use crate::domain::proxy::chain::TenantChain;
use crate::domain::proxy::plugins::{self as plugin_pipeline, PluginChain, upstream_ref};
use crate::domain::proxy::{breaker, cors, headers, ratelimit, routing};
use crate::domain::store::Store;
use crate::domain::validation::{check_egress, validate_framing};
use crate::error::{ERROR_SOURCE_UPSTREAM, OagwError, OagwErrorKind, OagwResult, ResourceKind};
use crate::infra::metrics;
use crate::infra::plugin::PluginRegistries;
use crate::infra::plugin::secrets::CredStore;
use crate::infra::plugin::traits::ErrorContext;
use crate::infra::plugin::traits::{PluginConfig, RequestContext, ResponseContext};

/// Boxed error of a streamed body (`axum::Error` compatible).
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Error surfaced to the client when a forwarded body cannot be completed.
///
/// The response head has already been sent at that point, so the failure can
/// only truncate the stream: the body ends early and the connection is
/// dropped, which the client sees as a broken framing rather than as a
/// complete response. The classification is logged for the operator.
#[derive(Clone, Copy, Debug, thiserror::Error)]
enum BodyFailure {
    /// A frame did not arrive within the idle budget.
    #[error("upstream body transfer timed out")]
    IdleTimeout,
    /// The transfer outlived the overall budget of a buffered body.
    #[error("upstream body transfer exceeded its budget")]
    Budget,
    /// The upstream connection failed mid-transfer.
    #[error("upstream body transfer failed")]
    Transfer,
}

impl BodyFailure {
    /// GTS slug of the failure, for the operator log.
    const fn slug(self) -> &'static str {
        match self {
            BodyFailure::IdleTimeout => "timeout.idle.v1",
            BodyFailure::Budget => "timeout.request.v1",
            BodyFailure::Transfer => "stream.aborted.v1",
        }
    }

    /// Error kind of the failure, as the circuit breaker classifies it.
    ///
    /// All three are the upstream failing to finish what it started, which is
    /// the evidence the breaker's window is for.
    const fn kind(self) -> OagwErrorKind {
        match self {
            BodyFailure::IdleTimeout => OagwErrorKind::IdleTimeout,
            BodyFailure::Budget => OagwErrorKind::RequestTimeout,
            BodyFailure::Transfer => OagwErrorKind::StreamAborted,
        }
    }
}

/// Streaming state of a forwarded upstream body.
struct BodyStream {
    body: ResponseBody,
    /// Longest silence tolerated between two frames.
    idle: Duration,
    /// Overall budget of the transfer; `None` for event streams.
    deadline: Option<Instant>,
    /// What the body tells the circuit breaker when it fails. The head of this
    /// very request has already reported, so a body that breaks is the second
    /// observation of the same request.
    report: breaker::BodyReport,
}

impl BodyStream {
    /// Next item of the stream, or `None` when the body is complete.
    ///
    /// The state is owned as [`Box<BodyStream>`] rather than by value so that a
    /// [`Frame::Data`] hands the **same** allocation back to [`StreamState`]:
    /// the box is what `unfold` moves from frame to frame, and rebuilding it
    /// here would cost one heap allocation and one deallocation per frame of
    /// every streamed body. Allocated once per body, in [`forward_body`].
    async fn next(mut self: Box<Self>) -> Option<(Result<Bytes, BoxError>, StreamState)> {
        if self.expired() {
            self.report_failure(BodyFailure::Budget);
            return Some(BodyStream::stop(BodyFailure::Budget));
        }
        match self.frame().await {
            Frame::Timeout => {
                self.report_failure(BodyFailure::IdleTimeout);
                Some(BodyStream::stop(BodyFailure::IdleTimeout))
            }
            Frame::End => None,
            Frame::Data(data) => Some((Ok(data), StreamState::Open(self))),
            Frame::Failed(error) => {
                tracing::warn!(error = %error, "upstream body transfer failed");
                self.report_failure(BodyFailure::Transfer);
                Some(BodyStream::stop(BodyFailure::Transfer))
            }
        }
    }

    /// Tell the breaker that this body never finished.
    ///
    /// The head reported the status the upstream answered with, so this is the
    /// second report of one request: the first counted the answer, this one
    /// counts that the upstream never finished giving it.
    fn report_failure(&self, failure: BodyFailure) {
        self.report.observe(failure.kind());
    }

    /// Close the stream after a failure: the head is already committed, so the
    /// body ends early and the framing breaks.
    fn stop(failure: BodyFailure) -> (Result<Bytes, BoxError>, StreamState) {
        tracing::warn!(slug = failure.slug(), error = %failure, "upstream body transfer aborted");
        (Err(BoxError::from(failure)), StreamState::Finished)
    }

    /// Whether the overall transfer budget ran out.
    fn expired(&self) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        let expired = Instant::now() >= deadline;
        if expired {
            tracing::warn!("upstream body exceeded its overall budget");
        }
        expired
    }

    /// Await the next frame under the idle timeout.
    async fn frame(&mut self) -> Frame {
        let Ok(frame) = tokio::time::timeout(self.idle, self.body.frame()).await else {
            return Frame::Timeout;
        };
        match frame {
            None => Frame::End,
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => Frame::Data(data),
                // A trailer ends the body; there is nothing to forward.
                Err(_) => Frame::End,
            },
            Some(Err(error)) => Frame::Failed(error),
        }
    }
}

/// Whether a forwarded body can still produce frames.
///
/// A body that reported a failure is **terminal**: the next poll of the stream
/// ends it instead of re-entering the error path, so a stalled upstream costs
/// one error item and not one per poll.
enum StreamState {
    /// More frames may arrive.
    ///
    /// Boxed because the state of a stream that carries a body, an idle budget
    /// and its report to the breaker is an order of magnitude larger than the
    /// terminal state next to it, and the enum is what `unfold` moves on every
    /// frame. The box is the state: [`BodyStream::next`] takes it, drives the
    /// body through it and hands the same allocation back, so the one
    /// allocation [`forward_body`] makes is the only one the body ever costs.
    Open(Box<BodyStream>),
    /// The transfer ended or failed; nothing more is forwarded.
    Finished,
}

/// Outcome of awaiting one upstream body frame.
enum Frame {
    /// A frame did not arrive within the idle budget.
    Timeout,
    /// The body is complete.
    End,
    /// A data frame to forward.
    Data(Bytes),
    /// The upstream connection failed mid-transfer.
    Failed(BoxError),
}

/// Streams an upstream body under the idle timeout.
///
/// `report` is what the body tells the circuit breaker when it fails: a 200
/// head followed by a body the upstream never finished is the slow-upstream
/// failure the head cannot see, and the head alone would record it as health.
fn forward_body(
    body: ResponseBody,
    idle: Duration,
    deadline: Option<Instant>,
    report: breaker::BodyReport,
) -> impl futures_util::Stream<Item = Result<Bytes, BoxError>> + Send + 'static {
    futures_util::stream::unfold(
        StreamState::Open(Box::new(BodyStream {
            body,
            idle,
            deadline,
            report,
        })),
        |state| async move {
            match state {
                StreamState::Open(stream) => stream.next().await,
                StreamState::Finished => None,
            }
        },
    )
}

/// Dial client of a WebSocket handshake: a plain HTTP/1.1 dialer whose
/// upgraded connection hands its two halves to the bridge task.
type HandshakeClient = LegacyClient<HttpConnector, Full<Bytes>>;

/// Proxy data plane of the gear.
pub struct ProxyService {
    store: Arc<dyn Store>,
    chain: Arc<dyn TenantChain>,
    client: HttpClient,
    /// Client of a WebSocket handshake (PRD session flows). Separate from
    /// `client` because it must not pool the connection it upgrades and must
    /// leave the upgraded socket untouched: the session has no budget.
    ws_client: HandshakeClient,
    /// Free slots of a live WebSocket session (PRD session flows). One permit
    /// per bridged session, taken before the dial and held for as long as the
    /// two sockets stay open, so the cap bounds what is actually running.
    sessions: Arc<tokio::sync::Semaphore>,
    /// Configured ceiling of [`ProxyService::sessions`], for the refusal detail.
    max_sessions: usize,
    /// Plugin registries of the data plane (ADR-0002): one per family, built
    /// once over the built-ins.
    plugins: Arc<PluginRegistries>,
    /// Round-robin cursors, one per upstream pool: `dashmap` keeps a cursor
    /// next to its pool without serialising unrelated upstreams.
    round_robin: DashMap<Uuid, AtomicUsize>,
    /// Token buckets of the data plane (ADR-0003), keyed by the resolved
    /// counter key. They live as long as the service does, so a quota survives
    /// the requests that spend it.
    buckets: ratelimit::Buckets,
    /// Circuit breakers of the data plane (PRD
    /// `cpt-cf-oagw-nfr-high-availability`), one per upstream id, for as long
    /// as the service does: an open breaker survives the requests that tripped
    /// it and outlives the cooldown.
    breakers: Arc<breaker::CircuitBreakers>,
    /// Budget of the dial plus the wait for the response head.
    head_timeout: Duration,
    /// Silence tolerated between two frames of a forwarded body.
    body_idle: Duration,
    /// Overall budget of a forwarded body that is not an event stream.
    body_stream: Duration,
    /// Hard request-body limit in bytes.
    max_body_bytes: u64,
    /// Whether plaintext upstream endpoints may be dialled.
    allow_http_upstream: bool,
    /// Server-side request forgery guards, re-applied at dial time.
    ssrf: SsrfPolicy,
    /// Instruments of DESIGN §4.2, emitted against the meter provider the host
    /// installs. Every emit is fire-and-forget: a lost data point is never a
    /// failed request.
    metrics: metrics::ProxyMetrics,
}

impl ProxyService {
    /// Build a data plane over `store`, `chain`, `client` and `credstore`.
    ///
    /// `credstore` is what the auth plugins resolve their `cred://` references
    /// through. A deployment that wires none still boots: the plugin registries
    /// degrade to the plugins that need no credential store, and an upstream
    /// whose binding needs one fails its requests closed (503
    /// `link.unavailable.v1`) instead of forwarding them unauthenticated.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        chain: Arc<dyn TenantChain>,
        client: HttpClient,
        credstore: Option<CredStore>,
        config: &OagwConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            chain,
            client,
            ws_client: LegacyClient::builder(TokioExecutor::new()).build_http::<Full<Bytes>>(),
            sessions: Arc::new(tokio::sync::Semaphore::new(config.max_websocket_sessions)),
            max_sessions: config.max_websocket_sessions,
            plugins: Arc::new(PluginRegistries::with_builtins(
                credstore,
                Some(Self::client_config(config)),
                config.token_cache_config(),
            )),
            round_robin: DashMap::new(),
            buckets: ratelimit::Buckets::default(),
            breakers: Arc::new(breaker::CircuitBreakers::new(
                &config.circuit_breaker,
                config.head_timeout(),
            )),
            head_timeout: config.head_timeout(),
            body_idle: config.body_idle_timeout(),
            body_stream: config.body_stream_timeout(),
            max_body_bytes: config.max_body_bytes,
            allow_http_upstream: config.allow_http_upstream,
            ssrf: config.ssrf_policy.clone(),
            metrics: metrics::ProxyMetrics::from_global(),
        })
    }

    /// Outbound client for `config`.
    ///
    /// Built **once** per configuration: no retries (the gateway must not
    /// re-send a client's request), no redirects (3xx pass through) and a
    /// transport decision that follows `oagw.config.allow_http_upstream`. The
    /// plaintext decision therefore belongs to the transport, not to the
    /// `scheme` enum, which always accepts `http`.
    ///
    /// # Errors
    /// 500 when the client cannot be constructed; under `--features fips` also
    /// when plaintext upstreams are allowed, which such a build rejects.
    pub fn build_client(config: &OagwConfig) -> OagwResult<HttpClient> {
        HttpClientBuilder::with_config(Self::client_config(config))
            .build()
            .map_err(|error| {
                OagwError::new(
                    OagwErrorKind::Internal,
                    format!("outbound HTTP client unavailable: {error}"),
                )
            })
    }

    /// HTTP client configuration of the data plane.
    ///
    /// Shared with the `OAuth2` token exchange, so an `IdP` behind the same
    /// egress policy is reached exactly the way an upstream is.
    fn client_config(config: &OagwConfig) -> HttpClientConfig {
        let mut http_config = HttpClientConfig::proxy();
        http_config.transport = if config.allow_http_upstream {
            TransportSecurity::AllowInsecureHttp
        } else {
            TransportSecurity::TlsOnly
        };
        http_config
    }

    /// The plugin registries of this data plane.
    #[must_use]
    pub fn plugins(&self) -> &PluginRegistries {
        &self.plugins
    }

    /// Proxy one request (DESIGN §3.5).
    ///
    /// # Errors
    /// Every problem of the DESIGN §3.3 data-plane table; a returned error is
    /// rendered as `X-OAGW-Error-Source: gateway`.
    pub async fn proxy(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        request_path: &str,
        request: http::Request<axum::body::Body>,
    ) -> OagwResult<axum::response::Response> {
        let (parts, body) = request.into_parts();
        let method = parts.method;
        let inbound = parts.headers;
        let query = parts.uri.query().map(str::to_owned);
        // The timer opens here, so `phase = total` is the whole of the request
        // the data plane saw: resolution, guards, dial and answer. A refusal is
        // a shorter request, not an untimed one.
        let started = Instant::now();
        // The handle of the *client* connection the platform armed for this
        // request: a 101 hands that socket over, so the data plane has to keep
        // it from the moment the request arrives.
        let client_upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();
        let target_host = inbound
            .get(routing::TARGET_HOST_HEADER)
            .and_then(|value| value.to_str().ok());

        // A CORS preflight is answered before anything is resolved: browser
        // preflights carry no credentials, so there is no tenant context to
        // resolve an upstream with and no plugin chain to run (ADR-0004
        // "Preflight Request Handling").
        if cors::is_preflight(&method, &inbound) {
            return Ok(cors::preflight(&inbound));
        }

        let upstream = self.resolve_upstream(ctx, alias).await?;
        let routes = self
            .store
            .list_routes_for_upstream(upstream.tenant_id, upstream.id)?;
        let selection = routing::select_route(&routes, &method, request_path)
            .ok_or_else(|| route_not_found(alias, request_path))?;
        // From here on the request has a host and a route to be attributed to,
        // which is what every instrument of DESIGN §4.2 is labelled with.
        // `http.route` is the matched prefix, not the raw request path: the
        // segments behind it are client input.
        let host = upstream.alias.as_str();
        let route = selection.http.path.as_str();
        let method_label = metrics::normalize_method(&method);
        // The gauge is per host, so a request only counts once it has one.
        let _in_flight = self.metrics.in_flight(host);
        // The chain is resolved before the guards, so a CORS or a rate limit
        // refusal is still enriched by the error-side plugins (DESIGN §3.3).
        let chain = plugin_pipeline::resolve_chain(
            &upstream,
            Some(selection.route),
            &self.plugins,
            self.store.as_ref(),
        )?;
        // CORS is scored before the rate limit, so a disallowed origin is
        // never counted against a quota (ADR-0004 "Actual Request Handling").
        let verdict = self
            .guard(ctx, &upstream, &selection, &method, &inbound)
            .await;
        let Verdict { cors, quota } = verdict;
        // The breaker is asked after the client-facing guards: a rate-limit
        // refusal is the client's own budget and the upstream was never asked,
        // so it must not consume a probe slot of a half-open breaker, and the
        // 429 it already earned is more specific than a breaker refusal.
        let admission = match &quota {
            Ok(_) => self.breakers.admit(
                upstream.id,
                &upstream.alias,
                std::time::Instant::now(),
                &self.metrics,
            ),
            Err(_) => breaker::Admit::Dial,
        };
        // The probe token is what proves the role this request was admitted as;
        // it is threaded down to the dial and the body, which are the two
        // observers that report, so a half-open breaker is moved only by the
        // request it is actually waiting for.
        let probe = admission.probe();
        // One exit, one measurement: a refusal and a dial failure are the same
        // outcome for the instruments of DESIGN §4.2, a request that resolved
        // to an upstream and a route and was answered with a status.
        let (granted, outcome) = match (quota, admission) {
            (Err(error), _) => (None, Err(error)),
            (Ok(quota), breaker::Admit::Refuse { retry_after_secs }) => (
                Some(quota),
                Err(breaker_refusal(&upstream.alias, retry_after_secs)),
            ),
            (Ok(quota), _) => (
                Some(quota),
                self.forward(
                    ctx,
                    &upstream,
                    &chain,
                    &selection,
                    &method,
                    &inbound,
                    query.as_deref(),
                    target_host,
                    client_upgrade.as_ref(),
                    probe,
                    body,
                )
                .await,
            ),
        };

        // On the success path the status is the upstream's. On the failure path
        // no upstream status exists, so the counter carries the status the
        // gateway answered with; `oagw_errors_total` marks those requests.
        let status = match &outcome {
            Ok(response) => response.status().as_u16(),
            Err(error) => error.status(),
        };
        self.metrics.request(host, method_label, route, status);
        self.metrics
            .duration(host, route, started.elapsed().as_secs_f64());
        match outcome {
            Ok(mut response) => {
                // An enabled policy is authoritative for its origins, so the
                // upstream's own CORS answer never reaches the client next to
                // the gateway's.
                if !cors.is_empty() {
                    cors::strip_upstream_headers(response.headers_mut());
                }
                let headers = response.headers_mut();
                headers.extend(cors);
                if let Some(quota) = granted {
                    headers.extend(quota);
                }
                Ok(response)
            }
            Err(error) => {
                self.metrics.error(host, route, &error.gts_type());
                let failed = self.error_phase(&chain, ctx, &upstream, error).await;
                Err(failed.with_cors_headers(&cors))
            }
        }
    }

    /// Gate a resolved request on its CORS policy and its quota.
    ///
    /// Both run after the upstream and the route are known and before anything
    /// is dialled or buffered, so a refusal costs neither an upstream call nor
    /// a body read. CORS first: a disallowed origin is not a client the quota
    /// should have to serve.
    ///
    /// Returns the headers a forwarded response carries for either guard.
    async fn guard(
        &self,
        ctx: &SecurityContext,
        upstream: &Upstream,
        selection: &routing::RouteSelection<'_>,
        method: &Method,
        inbound: &HeaderMap,
    ) -> Verdict {
        let route = selection.route;
        // One walk serves both steps: the chain is the same, so a request that
        // declares a policy anywhere in it pays a single store read for it. A
        // request with no policy at all walks nothing.
        let declared = route.rate_limit.is_some()
            || route.cors.is_some()
            || upstream.rate_limit.is_some()
            || upstream.cors.is_some();
        let ancestors = if declared {
            self.ancestor_upstreams(ctx, upstream)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let cors = match cors_step(upstream, route, method, inbound, &ancestors) {
            Ok(headers) => headers,
            // The refusal carries its own CORS answer already.
            Err(error) => {
                return Verdict {
                    cors: HeaderMap::new(),
                    quota: Err(error),
                };
            }
        };
        Verdict {
            cors,
            quota: self
                .quota(ctx, upstream, selection, inbound, &ancestors)
                .await,
        }
    }

    /// Score the request against the effective limit of its chain.
    ///
    /// The route's own policy is the most specific level (ADR-0003 Example 3)
    /// and the `enforce` caps of the ancestor upstreams stay active on top of
    /// it.
    ///
    /// # Errors
    /// 429 when the bucket is empty and the strategy does not tolerate it.
    async fn quota(
        &self,
        ctx: &SecurityContext,
        upstream: &Upstream,
        selection: &routing::RouteSelection<'_>,
        inbound: &HeaderMap,
        ancestors: &[Upstream],
    ) -> OagwResult<HeaderMap> {
        let route = selection.route;
        let Some(limit) = ratelimit::effective_limit(&rate_levels(route, upstream, ancestors))
        else {
            return Ok(HeaderMap::new());
        };
        let key = ratelimit::counter_key(
            upstream.id,
            &limit,
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            route.id,
            forwarded_for(inbound),
        );
        let outcome = match ratelimit::enforce(&self.buckets, &key, &limit, self.head_timeout).await
        {
            Ok(outcome) => outcome,
            // The 429 decision point: the state the limiter gave up on is the
            // one fact the usage gauge can carry, and `path` is the matched
            // prefix, since the raw request path is unbounded cardinality.
            Err(refusal) => {
                self.metrics
                    .rate_limit_exceeded(&upstream.alias, &selection.http.path);
                self.metrics.rate_limit_usage(
                    &upstream.alias,
                    &selection.http.path,
                    refusal.usage_ratio(),
                );
                return Err(refusal.error);
            }
        };
        Ok(if limit.response_headers {
            ratelimit::headers(&outcome)
        } else {
            HeaderMap::new()
        })
    }

    /// The same-alias upstreams of the tenant chain above the resolved one,
    /// nearest first.
    ///
    /// Ancestor records that are disabled or that resolve to nothing
    /// contribute nothing: an ancestor that is not routable cannot bind a
    /// budget either.
    async fn ancestor_upstreams(
        &self,
        ctx: &SecurityContext,
        upstream: &Upstream,
    ) -> OagwResult<Vec<Upstream>> {
        let wanted = crate::domain::alias::normalize(&upstream.alias);
        let mut records = Vec::new();
        for tenant in self.chain.ancestors(ctx, upstream.tenant_id).await? {
            if let Some(record) = self.store.find_upstream_by_alias(tenant, &wanted)?
                && record.enabled
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Forward one gated request to the upstream it resolved to.
    ///
    /// # Errors
    /// Every problem of the DESIGN §3.3 data-plane table from the framing
    /// validation on.
    #[allow(
        clippy::too_many_arguments,
        reason = "the tail of the pipeline is one step and takes what the earlier phases gathered"
    )]
    async fn forward(
        &self,
        ctx: &SecurityContext,
        upstream: &Upstream,
        chain: &PluginChain,
        selection: &routing::RouteSelection<'_>,
        method: &Method,
        inbound: &HeaderMap,
        query: Option<&str>,
        target_host: Option<&str>,
        client_upgrade: Option<&hyper::upgrade::OnUpgrade>,
        probe: Option<breaker::Probe>,
        body: axum::body::Body,
    ) -> OagwResult<axum::response::Response> {
        let declared = self.framing(inbound)?;
        // A handshake is recognised before the body is read: what follows its
        // head decides whether the request can be a handshake at all.
        let handshake = headers::is_websocket_handshake(method, inbound);
        let body = self.read_body(body, declared).await?;
        // A handshake carries no body (RFC 6455 §4.1): bytes after its head are
        // not part of it, and dialling them as if they were would leave the
        // handshake client waiting for a body it is never given.
        if handshake && !body.is_empty() {
            return Err(OagwError::validation(
                "a websocket handshake carries no body",
            ));
        }
        let picked =
            routing::select_endpoint(upstream, target_host, || self.next_index(upstream.id))?;
        let endpoint = picked.endpoint;
        self.check_egress(endpoint)?;
        // A handshake is the one request whose answer switches protocols, so it
        // has to be dialled with a client that can hand the socket over. Every
        // refusal below happens before a byte is dialled or a plugin runs.
        let session = if handshake {
            check_upgradable(endpoint)?;
            if client_upgrade.is_none() {
                return Err(upgrade_unavailable(
                    "the platform did not offer this connection an upgrade",
                ));
            }
            // The slot is taken before the dial and held until the session
            // ends, so a handshake that never opens cannot squat a permit
            // either: it is bounded by the head budget instead.
            Some(self.take_session_slot()?)
        } else {
            None
        };
        let path = routing::upstream_path(selection.http, &selection.suffix)?;
        let filtered = routing::filter_query(&selection.http.query_allowlist, query);
        let mut outbound = headers::outbound_request_headers(
            inbound,
            upstream
                .headers
                .as_ref()
                .and_then(|rules| rules.request.as_ref()),
            byte_len(&body),
        )?;
        // The one exemption of the strip list: a handshake keeps the two
        // headers the upgrade is made of (DESIGN §3.2 header table).
        if handshake {
            headers::restore_upgrade_headers(&mut outbound, inbound);
        }
        set_authority(&mut outbound, endpoint)?;

        // The plugin chain sees the header set that is about to be dialled and
        // may still rewrite the query (DESIGN §3.2: "plugin mutable"). It runs
        // after the header rules, so an injected credential is never dropped
        // again, and before the URL is finalised, so a credential written into
        // the query is part of the request that is signed off.
        let query_before = filtered.clone().unwrap_or_default();
        let mut request = RequestContext {
            security: ctx.clone(),
            upstream: upstream_ref(upstream.id, &upstream.alias),
            method: method.clone(),
            headers: std::mem::take(&mut outbound),
            query: query_before,
            config: PluginConfig::empty(),
        };
        self.run_request_phase(chain, &mut request).await?;
        let forwarded_query = if request.query == filtered.clone().unwrap_or_default() {
            filtered
        } else {
            Some(request.query)
        };
        // From here on a failure is a problem document the error-side plugins
        // may still enrich; the failure itself is never theirs to replace.
        let url = routing::target_url(
            endpoint,
            &selection.http.path,
            &path,
            forwarded_query.as_deref(),
        )?;
        if handshake {
            let client_upgrade = client_upgrade.cloned().unwrap_or_else(|| {
                unreachable!("a handshake reached the dial without an upgrade handle")
            });
            // Applied a second time, so a plugin request phase cannot break the
            // handshake any more than a header rule can (DESIGN §3.2). The
            // values are canonical either way, which is why re-applying cannot
            // undo what a plugin legitimately added elsewhere.
            headers::restore_upgrade_headers(&mut request.headers, inbound);
            // `session` is `Some` here by the same argument as above; dropping
            // it releases the slot, which is what a non-101 answer wants: the
            // session it was taken for never happens.
            self.dial(upstream, &picked);
            let handshake = self.send_handshake(url.as_str(), request.headers).await;
            self.report(
                upstream,
                handshake.as_ref().map(http::Response::status),
                probe,
            );
            let response = handshake?;
            // The upstream decides: a 101 switches protocols, anything else is
            // an ordinary answer the client reads as it would have without the
            // gateway — the refused handshake is exactly that.
            return if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
                self.switch_protocols(response, upstream, chain, ctx, client_upgrade, session)
                    .await
            } else {
                self.respond(response.map(streamed), upstream, chain, ctx, probe)
                    .await
            };
        }
        self.dial(upstream, &picked);
        let dial = self.send(method, url.as_str(), request.headers, body).await;
        self.report(upstream, dial.as_ref().map(HttpResponse::status), probe);
        let response = dial?;
        self.respond(response.into_inner(), upstream, chain, ctx, probe)
            .await
    }

    /// Tell the breaker what one dialled request saw of its upstream.
    ///
    /// Only a request that dialled reports, and the classification is
    /// [`breaker::is_health_failure`]: the status the upstream answered with, or
    /// the reason the dial failed. Nothing else reports — a refusal the gateway
    /// answered before the dial is not evidence about the upstream. `probe` is
    /// the token the request was admitted with, which is what lets a half-open
    /// breaker tell its own probe from a request that was dialled earlier.
    fn report(
        &self,
        upstream: &Upstream,
        answered: Result<http::StatusCode, &OagwError>,
        probe: Option<breaker::Probe>,
    ) {
        let observed = match answered {
            Ok(status) => breaker::Observed::Answered(status),
            Err(error) => breaker::Observed::Failed(*error.kind()),
        };
        self.breakers.record(
            upstream.id,
            &upstream.alias,
            probe,
            observed,
            std::time::Instant::now(),
            &self.metrics,
        );
    }

    /// Record the endpoint the request is about to dial (DESIGN §4.2).
    ///
    /// Both counters are dial counters, so they are emitted at the dial itself
    /// and not at the selection: a request the egress policy, the upgrade check
    /// or the plugin phase refused never dialled anything. The target-host
    /// counter counts only the requests that pinned their endpoint, since a
    /// round-robin or a single-endpoint dial used no header at all.
    fn dial(&self, upstream: &Upstream, picked: &routing::Selection<'_>) {
        let method = picked.method.as_str();
        self.metrics
            .endpoint_selected(upstream.id, &picked.endpoint.host, method);
        if picked.method == routing::SelectionMethod::ExplicitHeader {
            self.metrics
                .target_host_used(upstream.id, &picked.endpoint.host);
        }
    }

    /// Run the error-side plugin phase over a gateway failure.
    ///
    /// The transforms may add to the problem's extensions; the status and the
    /// detail the gateway decided on are reported unchanged
    /// ([`plugin_pipeline::run_error_phase`] enforces that).
    async fn error_phase(
        &self,
        chain: &PluginChain,
        ctx: &SecurityContext,
        upstream: &Upstream,
        error: OagwError,
    ) -> OagwError {
        let mut context = ErrorContext {
            security: ctx.clone(),
            upstream: upstream_ref(upstream.id, &upstream.alias),
            error,
            config: PluginConfig::empty(),
        };
        plugin_pipeline::run_error_phase(chain, &mut context).await;
        context.error
    }

    /// Run the request-side plugin phase under the head budget.
    ///
    /// A hung credential source must not hang the request: the phase shares the
    /// budget of the response head, which is also what the dial itself gets.
    async fn run_request_phase(
        &self,
        chain: &PluginChain,
        ctx: &mut RequestContext,
    ) -> OagwResult<()> {
        match tokio::time::timeout(
            self.head_timeout,
            plugin_pipeline::run_request_phase(chain, ctx),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err(OagwError::new(
                OagwErrorKind::RequestTimeout,
                format!(
                    "the plugin chain did not finish within {:?}",
                    self.head_timeout
                ),
            )),
        }
    }

    /// Egress guard of the selected endpoint (DESIGN §4.4).
    ///
    /// Re-applies the deployment's SSRF policy to the endpoint that is about to
    /// be dialled. The policy is off in the graded configuration, in which case
    /// this adds no rejection.
    ///
    /// # Errors
    /// 503 when the policy refuses the host.
    fn check_egress(&self, endpoint: &Endpoint) -> OagwResult<()> {
        if let Err(error) = check_egress(&self.ssrf, &endpoint.host) {
            tracing::warn!(host = %endpoint.host, "upstream endpoint refused by the egress policy");
            return Err(error);
        }
        if routing::plaintext_disallowed(endpoint, self.allow_http_upstream) {
            return Err(OagwError::new(
                OagwErrorKind::ProtocolError,
                format!(
                    "plaintext upstream schemes require oagw.config.allow_http_upstream ({})",
                    endpoint.scheme.as_str()
                ),
            )
            .with_extension(|ext| {
                ext.host = Some(endpoint.host.clone());
                ext.invalid_value = Some(endpoint.scheme.as_str().to_owned());
            }));
        }
        Ok(())
    }

    /// Resolve the alias across the tenant chain (DESIGN §3.2 "Shadowing").
    ///
    /// The alias is matched case-insensitively and without a trailing dot
    /// (DESIGN §3.2 "Alias Resolution": `Api.OpenAI.COM` names the same
    /// upstream). The walk starts at the calling tenant and follows the
    /// hierarchy to the root; the closest match wins. A disabled upstream is
    /// never routable and does **not** let the walk continue past it, so an
    /// ancestor-disabled upstream stays disabled for every descendant (PRD
    /// `fr-enable-disable`).
    ///
    /// Routes are read from the tenant that owns the resolved upstream: the
    /// control plane only accepts a binding whose upstream and route live in
    /// the same tenant, and the walk has already established that the caller
    /// may reach that tenant.
    async fn resolve_upstream(&self, ctx: &SecurityContext, alias: &str) -> OagwResult<Upstream> {
        let wanted = crate::domain::alias::normalize(alias);
        let tenant_id = ctx.subject_tenant_id();
        let mut walked = vec![tenant_id];
        walked.extend(self.chain.ancestors(ctx, tenant_id).await?);
        for candidate in walked {
            if let Some(upstream) = self.store.find_upstream_by_alias(candidate, &wanted)? {
                if !upstream.enabled {
                    return Err(OagwError::new(
                        OagwErrorKind::LinkUnavailable,
                        format!("upstream '{wanted}' is disabled"),
                    )
                    .with_extension(|ext| {
                        ext.alias = Some(wanted.clone());
                    }));
                }
                return Ok(upstream);
            }
        }
        // The data plane has a single 404 contract (DESIGN §3.3): no route
        // matched, which covers an alias that resolves to no upstream too.
        Err(route_not_found(&wanted, ""))
    }

    /// Next round-robin index of an endpoint pool.
    fn next_index(&self, upstream_id: Uuid) -> usize {
        let cursor = self.round_robin.entry(upstream_id).or_default();
        cursor.fetch_add(1, Ordering::Relaxed)
    }

    /// Drop the per-upstream state of a deleted upstream.
    ///
    /// Without this the cursor table would grow by one entry per upstream ever
    /// created; the cursor itself carries no configuration, so forgetting it
    /// only restarts the rotation. The token buckets **do** carry a budget, so
    /// they go with the record: a recreated upstream would otherwise inherit a
    /// spent one (ADR-0003 "Distribution").
    fn forget_upstream(&self, upstream_id: Uuid) {
        self.round_robin.remove(&upstream_id);
        self.buckets.forget_upstream(upstream_id);
        self.breakers.forget(upstream_id);
    }

    /// Validate the declared framing before a byte is buffered.
    fn framing(&self, inbound: &HeaderMap) -> OagwResult<Option<u64>> {
        let lengths: Vec<&str> = inbound
            .get_all(http::header::CONTENT_LENGTH)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        let encoding = inbound
            .get(http::header::TRANSFER_ENCODING)
            .and_then(|value| value.to_str().ok());
        validate_framing(&lengths, encoding, self.max_body_bytes)
    }

    /// Buffer the request body under the configured cap and re-check its
    /// declared length.
    ///
    /// The cap is applied **while** the frames arrive, not after: a body that
    /// never declares its length is cut off as soon as it passes
    /// `max_body_bytes`, so an unbounded upload cannot fill the gateway's
    /// memory (DESIGN §3.2 "Body Validation Rules").
    async fn read_body(
        &self,
        mut body: axum::body::Body,
        declared: Option<u64>,
    ) -> OagwResult<Bytes> {
        let mut buffered = BytesMut::new();
        loop {
            let Some(frame) = body.frame().await else {
                break;
            };
            let data = frame
                .map_err(|error| {
                    OagwError::validation(format!("request body could not be read: {error}"))
                })?
                .into_data()
                .map_err(|_| OagwError::validation("request body carried a non-data frame"))?;
            buffered.extend_from_slice(&data);
            if byte_len(&buffered) > self.max_body_bytes {
                return Err(OagwError::payload_too_large(
                    self.max_body_bytes,
                    byte_len(&buffered),
                ));
            }
        }
        let bytes = buffered.freeze();
        if let Some(declared) = declared
            && byte_len(&bytes) != declared
        {
            return Err(OagwError::validation(
                "request body does not match its declared Content-Length",
            ));
        }
        Ok(bytes)
    }

    /// Dial the upstream once, under the configured timeout.
    ///
    /// A timeout, a transport failure or a TLS failure is never retried: the
    /// client carries no retry policy and the gateway does not re-send a
    /// client's request (PRD `fr-request-proxy`).
    ///
    /// The budget covers the dial **and** the wait for the response head. The
    /// outbound client exposes no separate connect budget and reports no
    /// connect-specific failure, so a stalled connection surfaces as
    /// `timeout.request.v1` rather than as `timeout.connection.v1`.
    async fn send(
        &self,
        method: &Method,
        url: &str,
        outbound: HeaderMap,
        body: Bytes,
    ) -> OagwResult<HttpResponse> {
        let builder = self.builder(method, url)?;
        let pending = builder
            .headers(header_pairs(&outbound)?)
            .body_bytes(body)
            .send();
        match tokio::time::timeout(self.head_timeout, pending).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(transport_error(&error)),
            Err(_) => Err(OagwError::new(
                OagwErrorKind::RequestTimeout,
                format!("upstream did not respond within {:?}", self.head_timeout),
            )),
        }
    }

    /// Request builder for a method the proxy can forward.
    ///
    /// Only the five methods the upstream schema's `methods` enum defines reach
    /// this point: `HEAD` and `OPTIONS` cannot match a route, so the router
    /// never asks for them.
    fn builder(&self, method: &Method, url: &str) -> OagwResult<RequestBuilder> {
        match method.as_str() {
            "GET" => Ok(self.client.get(url)),
            "POST" => Ok(self.client.post(url)),
            "PUT" => Ok(self.client.put(url)),
            "DELETE" => Ok(self.client.delete(url)),
            "PATCH" => Ok(self.client.patch(url)),
            other => Err(OagwError::validation(format!(
                "method '{other}' cannot be proxied"
            ))),
        }
    }

    /// Stream the upstream response back to the client.
    ///
    /// The response-side plugin phase runs **before** the body is streamed, so
    /// a guard rejection is still a 502 the client sees instead of a body that
    /// breaks halfway. An event stream has **no** overall budget: it may pause
    /// for a long time as long as it keeps producing. Every other body is
    /// bounded in total. Both are bounded in silence.
    ///
    /// `probe` is the role the request was admitted as, carried so the body —
    /// the second observer of this request — reports to the same breaker with
    /// the same proof of role the head reported with.
    async fn respond(
        &self,
        response: http::Response<ResponseBody>,
        upstream: &Upstream,
        chain: &PluginChain,
        ctx: &SecurityContext,
        probe: Option<breaker::Probe>,
    ) -> OagwResult<axum::response::Response> {
        let status = response.status();
        let (parts, body) = response.into_parts();
        let upstream_headers = parts.headers;
        let mut outbound = headers::outbound_response_headers(
            &upstream_headers,
            upstream
                .headers
                .as_ref()
                .and_then(|rules| rules.response.as_ref()),
            ERROR_SOURCE_UPSTREAM,
        )?;
        if !chain.is_empty() {
            let mut phase = ResponseContext {
                security: ctx.clone(),
                upstream: upstream_ref(upstream.id, &upstream.alias),
                status,
                headers: outbound,
                upstream_headers: upstream_headers.clone(),
                config: PluginConfig::empty(),
            };
            plugin_pipeline::run_response_phase(chain, &mut phase).await?;
            outbound = phase.headers;
        }
        let event_stream = headers::is_event_stream_content(upstream_headers.get(CONTENT_TYPE));
        let deadline = (!event_stream).then(|| Instant::now() + self.body_stream);
        // The body outlives this call, so what it reports to has to be owned:
        // the breakers behind an `Arc`, the upstream it came from and the role
        // its request was admitted as.
        let report = breaker::BodyReport::for_response(
            Arc::clone(&self.breakers),
            upstream,
            probe,
            self.metrics.clone(),
        );
        let stream = forward_body(body, self.body_idle, deadline, report);
        let mut response = axum::response::Response::builder()
            .status(status)
            .body(axum::body::Body::from_stream(stream))
            .map_err(|error| {
                OagwError::new(
                    OagwErrorKind::Internal,
                    format!("upstream response could not be forwarded: {error}"),
                )
            })?;
        *response.headers_mut() = outbound;
        Ok(response)
    }

    /// Dial the handshake of a WebSocket session.
    ///
    /// The budget covers the dial and the wait for the answer, which is the
    /// same rule the response head of a buffered request gets. The session that
    /// starts with the answer has **no** budget at all: it is not a body, it is
    /// a socket the client owns from then on.
    ///
    /// # Errors
    /// 503 when the upstream cannot be reached, 408 on a head that never came.
    async fn send_handshake(&self, url: &str, outbound: HeaderMap) -> OagwResult<Incoming> {
        let mut request = http::Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Full::new(Bytes::new()))
            .map_err(|error| {
                OagwError::new(
                    OagwErrorKind::Internal,
                    format!("the handshake request could not be built: {error}"),
                )
            })?;
        *request.headers_mut() = outbound;
        let pending = self.ws_client.request(request);
        match tokio::time::timeout(self.head_timeout, pending).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "upstream handshake failed");
                Err(OagwError::new(
                    OagwErrorKind::LinkUnavailable,
                    format!("upstream request failed: {error}"),
                ))
            }
            Err(_) => Err(OagwError::new(
                OagwErrorKind::RequestTimeout,
                format!("upstream did not respond within {:?}", self.head_timeout),
            )),
        }
    }

    /// Hand the client socket over and bridge it to the upstream one.
    ///
    /// The acceptance of the session is judged first (RFC 6455 §4.2.2): a 101
    /// that names no `websocket` upgrade and no `Sec-WebSocket-Accept` is not a
    /// session, and the gateway still owns the answer, so the client is given a
    /// problem document instead of a socket it can never use. The answer head
    /// then goes out (the response-side plugins may still add to it, and the
    /// upstream's `Sec-WebSocket-*` values are forwarded verbatim); the two
    /// sockets are joined in a task of their own, because the session outlives
    /// the request by design. Once the head is gone a live session cannot carry
    /// a problem document, so whatever ends *that* session is a log record,
    /// never a client-visible error (ADR-0007).
    ///
    /// `session` is the slot the handshake took; the bridge holds it, so the
    /// cap counts what is actually running and the slot is freed the moment the
    /// session ends.
    async fn switch_protocols(
        &self,
        response: Incoming,
        upstream: &Upstream,
        chain: &PluginChain,
        ctx: &SecurityContext,
        client_upgrade: hyper::upgrade::OnUpgrade,
        session: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> OagwResult<axum::response::Response> {
        let status = response.status();
        let upstream_headers = response.headers().clone();
        // hyper arms the response upgrade from the status alone, so without this
        // check a bare 101 would hand the client a "session" whose first read is
        // EOF. `ProtocolError`, not `LinkUnavailable`: the link dialled fine and
        // the upstream answered — it declined the switch its own status offered,
        // which is the upstream behaving wrongly, not the link being down. The
        // slot the handshake took is dropped with this return, so a handshake the
        // upstream refused holds no permit.
        if let Some(reason) = headers::rejected_upgrade_reason(&upstream_headers) {
            tracing::info!(
                alias = %upstream.alias,
                reason = %reason,
                "websocket session refused"
            );
            return Err(OagwError::new(OagwErrorKind::ProtocolError, reason));
        }
        let mut outbound = headers::outbound_response_headers(
            &upstream_headers,
            upstream
                .headers
                .as_ref()
                .and_then(|rules| rules.response.as_ref()),
            ERROR_SOURCE_UPSTREAM,
        )?;
        if !chain.is_empty() {
            let mut phase = ResponseContext {
                security: ctx.clone(),
                upstream: upstream_ref(upstream.id, &upstream.alias),
                status,
                headers: outbound,
                upstream_headers: upstream_headers.clone(),
                config: PluginConfig::empty(),
            };
            plugin_pipeline::run_response_phase(chain, &mut phase).await?;
            outbound = phase.headers;
        }
        outbound.insert(
            http::header::CONNECTION,
            HeaderValue::from_static(UPGRADE_VALUE),
        );
        outbound.insert(
            http::header::UPGRADE,
            HeaderValue::from_static(WEBSOCKET_VALUE),
        );
        // An upgraded head carries no `x-oagw-error-source`: ADR-0007 marks
        // error provenance, and a successful switch of protocols is neither an
        // error nor the gateway's answer.
        outbound.remove("x-oagw-error-source");
        let mut answer = axum::response::Response::builder()
            .status(status)
            .body(axum::body::Body::empty())
            .map_err(|error| {
                OagwError::new(
                    OagwErrorKind::Internal,
                    format!("the switched answer could not be built: {error}"),
                )
            })?;
        *answer.headers_mut() = outbound;
        let alias = upstream.alias.clone();
        let budget = self.head_timeout;
        tokio::spawn(
            bridge(client_upgrade, response, alias.clone(), budget, session).instrument(
                // The session outlives the request, so the log has to name it on
                // its own: an alias may be shadowed across tenants, an id may not.
                tracing::info_span!(
                    "websocket_bridge",
                    upstream = %upstream.id,
                    tenant = %upstream.tenant_id,
                    alias = %alias
                ),
            ),
        );
        Ok(answer)
    }

    /// Take one of the slots a live WebSocket session may occupy.
    ///
    /// A slot is taken before the dial and released when the session ends, so
    /// the cap counts the sessions that are actually bridging and not the
    /// handshakes that were merely answered.
    ///
    /// # Errors
    /// 503 `link.unavailable.v1` when every slot is already held.
    fn take_session_slot(&self) -> OagwResult<tokio::sync::OwnedSemaphorePermit> {
        self.sessions.clone().try_acquire_owned().map_err(|_| {
            upgrade_unavailable(&format!(
                "no free websocket session slot; the limit is {}",
                self.max_sessions
            ))
        })
    }
}

/// `Connection` value of a switched answer (RFC 9110 §7.6.1).
const UPGRADE_VALUE: &str = "upgrade";

/// `Upgrade` value of a switched WebSocket answer (RFC 6455 §4.1).
const WEBSOCKET_VALUE: &str = "websocket";

/// Body type of an upstream answer that has not been upgraded yet.
type Incoming = http::Response<hyper::body::Incoming>;

/// Box the body of an answer the handshake client handed back.
///
/// The ordinary response path streams a `toolkit_http::ResponseBody`, and the
/// handshake dial returns a plain hyper body; only the error type needs help,
/// because hyper's own error has to become the boxed one the stream carries.
fn streamed(body: hyper::body::Incoming) -> ResponseBody {
    body.map_err(|error| Box::new(error) as BoxError).boxed()
}

/// Bridge a live WebSocket session between the client and the upstream.
///
/// Both halves are the upgraded sockets: the client's, which the platform
/// handed the request to, and the upstream's, which the dial handed back. Each
/// direction is copied until it ends, and the far end is shut down when it
/// does, which is what makes a half-close propagate instead of hanging the
/// session. The upstream response is kept alive for the whole of the session,
/// because dropping it before the upgrade is taken cancels the socket.
///
/// A hyper socket speaks hyper's IO traits, so each half is wrapped in
/// [`hyper_util::rt::TokioIo`] before it can be split into a readable and a
/// writable side; the four sides then copy in two independent directions. A
/// session that never opened is logged, not surfaced: the client got the answer
/// head already, and there is nothing left to answer with (ADR-0007).
///
/// `session` is the slot the handshake took and is dropped with the task, so
/// the cap counts exactly the sessions that are running.
async fn bridge(
    client_upgrade: hyper::upgrade::OnUpgrade,
    mut response: Incoming,
    alias: String,
    budget: Duration,
    session: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    match handed_over(client_upgrade, &mut response, budget).await {
        Ok((client, upstream)) => hold(client, upstream, &alias).await,
        // The client has the answer head already, so the reason lives in the
        // log and nowhere the client could read (ADR-0007).
        Err(reason) => {
            tracing::info!(alias = %alias, reason = %reason, "websocket session never opened");
        }
    }
    // Released exactly when the session ends, whether it opened or not.
    drop(session);
}

/// Join the two upgraded halves of a session, or say why they never came.
///
/// The wait shares the head budget: a session whose upstream never hands its
/// socket over must end instead of holding its slot for ever. On expiry both
/// upgrade futures are dropped — the client's among them, which is what closes
/// the client's half, so the client sees the session end rather than hang.
///
/// The upstream response has to stay borrowed for the whole wait: dropping it
/// before its upgrade is taken cancels the socket instead of handing it over.
///
/// # Errors
/// The reason the two sockets never met, for the log only.
async fn handed_over(
    client_upgrade: hyper::upgrade::OnUpgrade,
    response: &mut Incoming,
    budget: Duration,
) -> Result<(hyper::upgrade::Upgraded, hyper::upgrade::Upgraded), String> {
    let upstream_upgrade = hyper::upgrade::on(response);
    let both = async { tokio::join!(client_upgrade, upstream_upgrade) };
    match tokio::time::timeout(budget, both).await {
        Ok((Ok(client), Ok(upstream))) => Ok((client, upstream)),
        Ok(ends) => Err(open_failure(ends)),
        Err(_) => Err(format!(
            "the socket was never handed over within {budget:?}"
        )),
    }
}

/// Hold both halves of a session and copy between them until it ends.
///
/// Each half is split into a readable and a writable side, so the two
/// directions can be copied independently: whichever ends first shuts its far
/// end down, which is what makes a half-close propagate.
async fn hold(client: hyper::upgrade::Upgraded, upstream: hyper::upgrade::Upgraded, alias: &str) {
    tracing::info!(alias = %alias, "websocket session opened");
    let (mut client_read, mut client_write) = tokio::io::split(TokioIo::new(client));
    let (mut upstream_read, mut upstream_write) = tokio::io::split(TokioIo::new(upstream));
    let (from_client, from_upstream) = tokio::join!(
        pipe(&mut client_read, &mut upstream_write, "client"),
        pipe(&mut upstream_read, &mut client_write, "upstream"),
    );
    tracing::info!(
        alias = %alias,
        client_bytes = from_client.copied(),
        upstream_bytes = from_upstream.copied(),
        "websocket session closed"
    );
    // The per-direction records are DEBUG: the closing one above already says
    // how much a session carried, and a busy gateway must not pay two INFO
    // records per direction for it.
    from_client.report(alias);
    from_upstream.report(alias);
}

/// Outcome of one direction of a bridged session.
struct Half {
    /// The side the direction read from: the client or the upstream.
    direction: &'static str,
    /// Bytes copied before the side ended, or the I/O error that ended it.
    copied: std::io::Result<u64>,
}

impl Half {
    /// Bytes the direction carried, for the closing record.
    #[must_use]
    fn copied(&self) -> u64 {
        self.copied.as_ref().copied().unwrap_or_default()
    }

    /// Record how the direction ended, at the level it deserves.
    fn report(&self, alias: &str) {
        match &self.copied {
            Ok(bytes) => {
                tracing::debug!(alias = %alias, direction = self.direction, bytes, "websocket session half ended");
            }
            Err(error) => {
                tracing::warn!(alias = %alias, direction = self.direction, %error, "websocket session half failed");
            }
        }
    }
}

/// Copy one direction of a session, then close the far end.
///
/// A `copy` that returns means the reading side ended: by the peer of that side
/// closing it — a close frame, then a half-close — or by an I/O error. Either
/// way the side that is left has to be shut down, or the session would wait on
/// a peer that has nothing more to say. `direction` names the side being read
/// from, which is the side that ended the half.
async fn pipe<R, W>(from: &mut R, to: &mut W, direction: &'static str) -> Half
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let copied = tokio::io::copy(from, to).await;
    close_half(direction, to.shutdown().await);
    Half { direction, copied }
}

/// Record the closing of one direction, which the session cannot report.
fn close_half(direction: &'static str, closed: std::io::Result<()>) {
    if let Err(error) = closed {
        tracing::warn!(direction, %error, "websocket session half could not be closed");
    }
}

/// Describe why a session never opened, for the log only.
///
/// Either half failing is enough to end the session, so the first `Err` is the
/// one that says why; the paired successes cannot reach this function.
fn open_failure(
    ends: (
        Result<hyper::upgrade::Upgraded, hyper::Error>,
        Result<hyper::upgrade::Upgraded, hyper::Error>,
    ),
) -> String {
    ends.0.err().or_else(|| ends.1.err()).map_or_else(
        || "the session was abandoned before it started".to_owned(),
        |error| format!("the socket was never handed over: {error}"),
    )
}

/// A refusal to carry a session: the gateway could not take the upgrade.
///
/// 503, because the request was well-formed and the route resolved; what is
/// missing is the gateway's own ability to hold a session open.
fn upgrade_unavailable(detail: &str) -> OagwError {
    tracing::warn!(detail, "websocket upgrade refused");
    OagwError::new(OagwErrorKind::LinkUnavailable, detail.to_owned())
}

/// Whether the selected endpoint can carry a bridged session.
///
/// The bridge dialer is a plaintext HTTP/1.1 client, because this slice builds
/// no second TLS configuration into the data plane: the graded deployment
/// (`config/e2e-local.yaml`, `allow_http_upstream: true`) is http-only, so a
/// TLS dialer would have nowhere to take its settings from. The deferred
/// requirement is the WebSocket/WebTransport session flow of `PRD.md:305`
/// (`cpt-cf-oagw-fr-streaming`); closing it needs a connector wired to the
/// deployment's TLS settings — `tokio-rustls` and `rustls` are already
/// workspace dependencies, so no new dependency is involved — not a change to
/// the bridge. A session therefore needs an `http`-scheme endpoint; any other
/// scheme is refused before a byte is dialled.
///
/// # Errors
/// 503 `link.unavailable.v1` naming the scheme that cannot be dialled.
fn check_upgradable(endpoint: &Endpoint) -> OagwResult<()> {
    if endpoint.scheme == crate::domain::model::Scheme::Http {
        return Ok(());
    }
    let scheme = endpoint.scheme.as_str();
    tracing::warn!(
        scheme,
        "websocket handshake refused: scheme cannot be bridged"
    );
    Err(upgrade_unavailable(&format!(
        "a websocket session needs an http upstream endpoint, not '{scheme}'"
    )))
}

/// `Content-Type` on the wire.
const CONTENT_TYPE: &http::HeaderName = &http::header::CONTENT_TYPE;

/// The verdict of the two guards that run before the dial.
///
/// The CORS headers are carried whatever the guards decided, because the answer
/// the client sees — a forwarded response, a 429 or a 403 — has to speak CORS
/// either way (ADR-0004 "Error Responses").
struct Verdict {
    /// CORS headers the answer carries, allowed or refused.
    cors: HeaderMap,
    /// Quota headers, when the request was admitted.
    quota: OagwResult<HeaderMap>,
}

/// `X-Forwarded-For` on the wire.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// The CORS levels of one request, descendant→ancestor.
///
/// `None` is a record that declares no `cors` member at all, which `inherit`
/// skips and `enforce` reads past — a member with an *empty* origin list is a
/// deny-all instead, and stays.
fn cors_levels<'a>(
    route: &'a crate::domain::model::Route,
    upstream: &'a Upstream,
    ancestors: &'a [Upstream],
) -> Vec<Option<&'a crate::domain::model::CorsConfig>> {
    let mut levels = vec![route.cors.as_ref(), upstream.cors.as_ref()];
    levels.extend(ancestors.iter().map(|record| record.cors.as_ref()));
    levels
}

/// Enforce the effective CORS policy of one actual request.
///
/// The origin and the method are checked here, on the request that will
/// carry credentials; the preflight above already told the browser the
/// request was worth making (ADR-0004 "Actual Request Handling").
///
/// # Errors
/// 403 for an origin or a method the effective policy does not allow. The
/// error carries the CORS headers the answer must show: none for an origin
/// that was never allowed, the permissive set for a method the policy
/// refuses.
fn cors_step(
    upstream: &Upstream,
    route: &crate::domain::model::Route,
    method: &Method,
    inbound: &HeaderMap,
    ancestors: &[Upstream],
) -> OagwResult<HeaderMap> {
    let policy = cors::effective(&cors_levels(route, upstream, ancestors));
    let Some(policy) = policy else {
        // A policy that is off, or that no record declares, adds no header
        // of its own (ADR-0004, deny by default).
        return Ok(HeaderMap::new());
    };
    if let Err(error) = cors::check(&policy, method, inbound) {
        let answer = match error.kind() {
            OagwErrorKind::CorsOriginNotAllowed => cors::denied_headers(),
            _ => cors::response_headers(&policy, origin(inbound)),
        };
        return Err(error.with_cors_headers(&answer));
    }
    Ok(cors::response_headers(&policy, origin(inbound)))
}

/// The rate-limit levels of one request, descendant→ancestor.
fn rate_levels<'a>(
    route: &'a crate::domain::model::Route,
    upstream: &'a Upstream,
    ancestors: &'a [Upstream],
) -> Vec<Option<&'a crate::domain::model::RateLimitConfig>> {
    let mut levels = vec![route.rate_limit.as_ref(), upstream.rate_limit.as_ref()];
    levels.extend(ancestors.iter().map(|record| record.rate_limit.as_ref()));
    levels
}

/// The `Origin` of a request, when it names one a browser sent.
fn origin(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
}

/// First hop of the forwarded chain.
///
/// The platform hands the gear no connection peer address, so the forwarded
/// chain is the only client identity available to an `ip`-scoped counter.
fn forwarded_for(headers: &HeaderMap) -> Option<&str> {
    let chain = headers.get(FORWARDED_FOR)?.to_str().ok()?;
    chain
        .split(',')
        .next()
        .map(str::trim)
        .filter(|hop| !hop.is_empty())
}

/// Point the outbound `Host` at the dial target (DESIGN §3.2 "Headers
/// Transformation").
///
/// The inbound `Host` names the gateway, not the upstream, and is stripped
/// with the other routing headers; the authority of the selected endpoint is
/// what the upstream expects to see.
fn set_authority(outbound: &mut HeaderMap, endpoint: &Endpoint) -> OagwResult<()> {
    let authority = routing::authority(endpoint);
    let value = HeaderValue::from_str(&authority).map_err(|_| {
        OagwError::validation(format!(
            "endpoint authority '{authority}' cannot become a Host header"
        ))
        .with_extension(|ext| ext.host = Some(endpoint.host.clone()))
    })?;
    outbound.insert(http::header::HOST, value);
    Ok(())
}

/// The data plane drops the per-upstream state of a deleted record.
impl UpstreamRemoval for ProxyService {
    fn upstream_removed(&self, upstream_id: Uuid) {
        self.forget_upstream(upstream_id);
    }
}

/// 404 for a request the data plane cannot route (DESIGN §3.3).
///
/// Both an alias that resolves to no upstream and a request no route of the
/// resolved upstream matches are reported as the single `route.not_found.v1`
/// contract, carrying the alias and — when known — the request path.
fn route_not_found(alias: &str, request_path: &str) -> OagwError {
    let detail = if request_path.is_empty() {
        format!("no upstream of the calling tenant answers to the alias '{alias}'")
    } else {
        format!("no route of upstream '{alias}' matches this request")
    };
    OagwError::new(OagwErrorKind::NotFound, detail)
        .with_resource(ResourceKind::Route)
        .with_extension(|ext| {
            ext.alias = Some(alias.to_owned());
            if !request_path.is_empty() {
                ext.path = Some(request_path.to_owned());
            }
        })
}

/// Length of a body in bytes, saturating at the `u64` maximum.
fn byte_len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// The answer of an open breaker: 503 `circuit_breaker.open.v1`, retriable, with
/// the seconds of cooldown still to run as the `Retry-After`.
///
/// Nothing is dialled and nothing is answered in the upstream's place (DESIGN
/// §4.7 leaves fallback strategies to a later slice): the client retries, which
/// is what `Retriable: Yes` asks of it.
fn breaker_refusal(alias: &str, retry_after_secs: u64) -> OagwError {
    // `debug!` and not `warn!`: a refusal is by definition high frequency — it
    // is what every request gets for the whole cooldown — and the audit record
    // the request is answered with is already at `WARN`. The transition the
    // breaker logs is the rare, operator-relevant one.
    tracing::debug!(host = %alias, retry_after_secs, "circuit breaker refused a request");
    OagwError::new(
        OagwErrorKind::CircuitBreakerOpen,
        format!("upstream '{alias}' is unhealthy: the circuit breaker is open"),
    )
    .with_extension(|ext| {
        ext.alias = Some(alias.to_owned());
        ext.retry_after_seconds = Some(retry_after_secs);
    })
}

/// Flatten a header map into the `(name, value)` pairs the client accepts.
///
/// A value that is not valid visible ASCII cannot be forwarded by the buffered
/// client; the request is rejected instead of being silently corrupted.
fn header_pairs(headers: &HeaderMap) -> OagwResult<Vec<(String, String)>> {
    headers
        .iter()
        .map(|(name, value)| {
            let rendered = value.to_str().map_err(|_| {
                OagwError::validation(format!(
                    "header '{name}' carries a value that cannot be forwarded"
                ))
            })?;
            Ok((name.as_str().to_owned(), rendered.to_owned()))
        })
        .collect()
}

/// Map a client failure onto the DESIGN §3.3 error table (ADR-0007).
fn transport_error(error: &HttpError) -> OagwError {
    let kind = transport_kind(error);
    tracing::warn!(kind = ?kind, error = %error, "upstream request failed");
    OagwError::new(kind, format!("upstream request failed: {error}"))
}

/// Classify a client failure.
fn transport_kind(error: &HttpError) -> OagwErrorKind {
    match error {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => OagwErrorKind::RequestTimeout,
        HttpError::Transport(_) | HttpError::Overloaded | HttpError::ServiceClosed => {
            OagwErrorKind::LinkUnavailable
        }
        HttpError::BodyTooLarge { .. } => OagwErrorKind::PayloadTooLarge,
        _ => OagwErrorKind::ProtocolError,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use http::HeaderValue;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::{
        ProxyService, byte_len, header_pairs, route_not_found, transport_error, transport_kind,
    };
    use crate::config::OagwConfig;
    use crate::domain::lifecycle::UpstreamRemoval as _;
    use crate::domain::model::{Endpoint, Scheme, Timestamps, Upstream};
    use crate::domain::proxy::chain::{NoChain, TenantChain};
    use crate::domain::store::{InMemoryStore, Store};
    use crate::error::OagwErrorKind;
    use toolkit_http::HttpError;

    const TENANT: Uuid = Uuid::nil();

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: Scheme::Http,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(alias: &str, endpoints: Vec<Endpoint>, enabled: bool) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: TENANT,
            alias: alias.to_owned(),
            enabled,
            protocol: crate::domain::model::Protocol::Http,
            endpoints,
            tags: Vec::new(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            timestamps: Timestamps {
                created_at: 0,
                updated_at: 0,
            },
        }
    }

    /// Chain that walks one level up to the root tenant.
    struct StaticChain;

    #[async_trait]
    impl TenantChain for StaticChain {
        async fn ancestors(
            &self,
            _ctx: &SecurityContext,
            tenant_id: Uuid,
        ) -> crate::error::OagwResult<Vec<Uuid>> {
            if tenant_id == Uuid::nil() {
                return Ok(Vec::new());
            }
            Ok(vec![Uuid::nil()])
        }
    }

    fn context(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous())
    }

    fn config() -> OagwConfig {
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        }
    }

    fn service(
        store: Arc<dyn Store>,
        chain: Arc<dyn TenantChain>,
        config: &OagwConfig,
    ) -> Arc<ProxyService> {
        let client = match ProxyService::build_client(config) {
            Ok(client) => client,
            Err(error) => panic!("client build must succeed in tests: {error}"),
        };
        ProxyService::new(store, chain, client, None, config)
    }

    fn seeded(alias: &str, enabled: bool) -> (Arc<ProxyService>, Uuid) {
        let record = upstream(alias, vec![endpoint("a.vendor.com", 443)], enabled);
        let id = record.id;
        let store = InMemoryStore::new();
        match store.insert_upstream(record) {
            Ok(_) => (),
            Err(error) => panic!("store insert must succeed: {error}"),
        }
        (service(store, Arc::new(NoChain), &config()), id)
    }

    async fn resolved(svc: &ProxyService, alias: &str, tenant: Uuid) -> Upstream {
        match svc.resolve_upstream(&context(tenant), alias).await {
            Ok(record) => record,
            Err(error) => panic!("alias '{alias}' must resolve: {error}"),
        }
    }

    #[tokio::test]
    async fn resolves_an_alias_in_the_calling_tenant() {
        let (svc, id) = seeded("api.vendor.com", true);
        assert_eq!(resolved(&svc, "api.vendor.com", TENANT).await.id, id);
    }

    #[tokio::test]
    async fn walks_the_tenant_chain_for_shadowing() {
        let config = config();
        let store = InMemoryStore::new();
        match store.insert_upstream(upstream(
            "shared",
            vec![endpoint("a.vendor.com", 443)],
            true,
        )) {
            Ok(_) => (),
            Err(error) => panic!("store insert must succeed: {error}"),
        }
        let svc = service(store, Arc::new(StaticChain), &config);
        assert_eq!(
            resolved(&svc, "shared", Uuid::now_v7()).await.tenant_id,
            TENANT
        );
    }

    #[tokio::test]
    async fn an_unknown_alias_is_a_not_found_problem() {
        let (svc, _) = seeded("api.vendor.com", true);
        let error = svc
            .resolve_upstream(&context(TENANT), "missing")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::NotFound);
        assert_eq!(error.status(), 404);
        assert_eq!(error.extensions().alias.as_deref(), Some("missing"));
    }

    #[tokio::test]
    async fn a_disabled_upstream_is_a_link_unavailable_problem() {
        let (svc, _) = seeded("down", false);
        let error = svc
            .resolve_upstream(&context(TENANT), "down")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::LinkUnavailable);
        assert_eq!(error.status(), 503);
    }

    #[tokio::test]
    async fn a_disabled_upstream_stops_the_walk() {
        let config = config();
        let store = InMemoryStore::new();
        match store.insert_upstream(upstream(
            "shared",
            vec![endpoint("a.vendor.com", 443)],
            false,
        )) {
            Ok(_) => (),
            Err(error) => panic!("store insert must succeed: {error}"),
        }
        let svc = service(store, Arc::new(StaticChain), &config);
        let error = svc
            .resolve_upstream(&context(Uuid::now_v7()), "shared")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::LinkUnavailable);
    }

    #[tokio::test]
    async fn round_robin_walks_the_pool() {
        let config = config();
        let svc = service(InMemoryStore::new(), Arc::new(NoChain), &config);
        let pool = Uuid::new_v4();
        assert_eq!(svc.next_index(pool), 0);
        assert_eq!(svc.next_index(pool), 1);
        assert_eq!(svc.next_index(Uuid::new_v4()), 0);
    }

    #[tokio::test]
    async fn a_removed_upstream_forgets_its_cursor() {
        let config = config();
        let svc = service(InMemoryStore::new(), Arc::new(NoChain), &config);
        let pool = Uuid::new_v4();
        assert_eq!(svc.next_index(pool), 0);
        assert_eq!(svc.next_index(pool), 1);

        // The control plane cascades the deletion to the data plane through
        // the lifecycle seam (DESIGN §3.6): the cursor of the pool goes away
        // with it, so a recreated pool starts at the first endpoint again.
        svc.upstream_removed(pool);
        assert_eq!(svc.next_index(pool), 0);
    }

    #[tokio::test]
    async fn a_removed_upstream_forgets_its_buckets() {
        use crate::domain::proxy::ratelimit;

        let config = config();
        let svc = service(InMemoryStore::new(), Arc::new(NoChain), &config);
        let upstream = Uuid::new_v4();
        let limit = ratelimit::Limit {
            rate: 1,
            window: "second".to_owned(),
            capacity: 1,
            scope: "tenant".to_owned(),
            strategy: ratelimit::Strategy::Reject,
            cost: 1,
            response_headers: true,
        };
        assert!(
            svc.buckets
                .score(
                    &ratelimit::counter_key(upstream, &limit, TENANT, TENANT, TENANT, None),
                    &limit,
                    1
                )
                .acquired
        );
        assert!(svc.buckets.holds(upstream));

        // The seam is the same one the round-robin cursor goes through: a
        // recreated upstream must not inherit a spent budget (ADR-0003
        // "Distribution").
        svc.upstream_removed(upstream);
        assert!(!svc.buckets.holds(upstream));
    }

    #[tokio::test]
    async fn client_builds_with_either_transport_switch() {
        assert!(ProxyService::build_client(&config()).is_ok());
        assert!(ProxyService::build_client(&OagwConfig::default()).is_ok());
    }

    #[test]
    fn byte_length_is_reported_in_bytes() {
        assert_eq!(byte_len(&[0u8; 3]), 3);
        assert_eq!(byte_len(&[]), 0);
    }

    #[test]
    fn header_pairs_reject_an_opaque_value() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-vendor",
            HeaderValue::from_bytes(&[0x80, 0x81]).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        assert!(header_pairs(&headers).is_err());
    }

    #[test]
    fn header_pairs_render_names_and_values() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-vendor", HeaderValue::from_static("1"));
        let pairs = header_pairs(&headers).unwrap_or_default();
        assert_eq!(pairs, vec![("x-vendor".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn transport_failures_map_onto_the_error_table() {
        let timeout = HttpError::Timeout(std::time::Duration::from_secs(1));
        assert_eq!(transport_kind(&timeout), OagwErrorKind::RequestTimeout);
        let refused = HttpError::Transport("connection refused".into());
        assert_eq!(transport_kind(&refused), OagwErrorKind::LinkUnavailable);
        let scheme = HttpError::InvalidScheme {
            scheme: "ftp".to_owned(),
            reason: "unsupported".to_owned(),
        };
        assert_eq!(transport_kind(&scheme), OagwErrorKind::ProtocolError);
        assert_eq!(transport_error(&refused).status(), 503);
        assert_eq!(
            transport_error(&timeout).kind(),
            &OagwErrorKind::RequestTimeout
        );
    }

    #[test]
    fn route_not_found_carries_the_alias_and_the_path() {
        let error = route_not_found("api.vendor.com", "/v1/chat");
        assert_eq!(error.status(), 404);
        assert_eq!(error.extensions().alias.as_deref(), Some("api.vendor.com"));
        assert_eq!(error.extensions().path.as_deref(), Some("/v1/chat"));
    }
}
