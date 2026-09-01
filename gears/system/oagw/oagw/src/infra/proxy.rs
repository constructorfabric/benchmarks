//! Outbound proxy engine of the OAGW data plane (DESIGN section 3.5).
//!
//! [`ProxyEngine`] owns the single shared upstream transport
//! (`pingora_core::connectors::TransportConnector`, ADR-0006) and turns a
//! resolved `(upstream, route, endpoint)` plus a client [`ProxyRequest`] into
//! an upstream HTTP/1.1 exchange. It is deliberately *protocol-only*: tenancy,
//! route resolution, CORS, rate limiting and the plugin chain live in the
//! handler, so the engine stays independently testable.
//!
//! Responsibilities
//!
//! * **endpoint selection** (ADR-0001): one endpoint wins outright, an explicit
//!   `x-oagw-target-host` is validated and matched case-insensitively, a
//!   common-suffix alias pool demands the header, and a distinct-host pool is
//!   load-balanced round-robin;
//! * **header pipeline** in both directions, with hop-by-hop stripping;
//! * **credential injection** (PRD §5.2): the headers and query parameters the
//!   plugin chain injected are applied *after* the client pipeline, so an
//!   injected credential replaces a caller-supplied value of the same name and
//!   survives a `passthrough: none` posture;
//! * **body validation before forwarding** (content length, transfer encoding,
//!   total size);
//! * **error mapping** onto the [`OagwError`] taxonomy;
//! * **streaming**: the response body is never buffered, so SSE and other
//!   long-lived streams flow through incrementally;
//! * **upgrade**: a WebSocket upgrade is forwarded and the two connections are
//!   spliced once both ends switch protocols.
//!
//! ## Documented deviations
//!
//! * The `proxy_timeout` budget covers connection establishment plus the
//!   request/response *header* exchange. Once headers arrive, the body streams
//!   without an overall deadline: an overall deadline would abort every SSE
//!   stream at a fixed wall-clock time.
//! * The engine does not hand connections back to pingora's reuse pool after
//!   an exchange, because the connection is owned by the spawned task that
//!   drives the hyper client. [`TransportConnector`] still provides the L4 and
//!   TLS timeouts, the TLS material and the socket discipline.
//! * A WebSocket upgrade is forwarded for `ws`/`wss` endpoints; a non-101
//!   answer is mapped to `502 ProtocolError` rather than streamed.
//! * **Upstream protocol version.** The engine speaks HTTP/1.1 upstream
//!   ([`hyper::client::conn::http1`] over the pingora
//!   [`TransportConnector`]), always. DESIGN §4.4 credits pingora with
//!   "adaptive per-host HTTP/2 detection"; that capability is not wired up
//!   here, so an HTTP/2-only upstream (and the `grpc` protocol, which the
//!   handler answers with `501`) is out of scope. Every outbound request is
//!   stamped `Version::HTTP_11` explicitly rather than negotiated.
//! * **Protocol switches.** Only a WebSocket upgrade is proxied, and the
//!   outbound handshake always signals `Connection: Upgrade` +
//!   `Upgrade: websocket`: the scheme allowlist (`ws`/`wss`) is what tells the
//!   engine a switch was meant, so a non-WebSocket `Upgrade` token (h2c,
//!   SPDY, a custom protocol) is treated as an ordinary header, stripped by the
//!   hop-by-hop filter and never forwarded. Upgrades to anything but
//!   `websocket`, and HTTP/2 extended CONNECT, are out of scope.
//! * **`http.response.status_code` label semantics.** The request instrument
//!   (`oagw_requests_total`) carries the *class* of the status — `2xx`, `3xx`,
//!   `4xx`, `5xx` (see [`crate::domain::metrics::status_class`]) — and not the
//!   numeric code. That is a deliberate deviation from the OpenTelemetry HTTP
//!   semantic conventions, which put the exact status in the attribute: the
//!   class keeps the series count bounded at four per `(host, route)` pair and
//!   is what DESIGN §4.2's dashboards aggregate on. A caller who needs the
//!   exact status reads it from the audit record (DESIGN §4.3), which carries
//!   the numeric value.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, Version};
use hyper_util::rt::TokioIo;
use pingora_core::connectors::{ConnectorOptions, TransportConnector};
use pingora_core::upstreams::peer::HttpPeer;
use uuid::Uuid;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::error::OagwError;
use crate::domain::metrics::MetricsRegistry;
use crate::domain::model::{Endpoint, HeaderRules, PassthroughMode, Route, Scheme, Upstream};
use crate::domain::validation::classify_host;

/// Header a client may set to pin the endpoint the gateway calls (ADR-0001).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Hard cap on the buffered request body.
pub const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;

/// RFC 9110 section 7.6.1 hop-by-hop headers, always stripped in both
/// directions (DESIGN section 3.3).
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
];

/// Header consumed by the routing layer and never forwarded upstream.
const CONSUMED: [&str; 1] = [TARGET_HOST_HEADER];

/// Transport headers the gateway always carries, whatever the passthrough mode
/// says. `Host` is set separately from the endpoint.
const OWNED: [&str; 2] = ["content-type", "content-length"];

/// Client headers a WebSocket handshake cannot complete without (RFC 6455
/// section 4.1). Forwarded verbatim on a protocol switch, independently of the
/// passthrough mode.
const WEBSOCKET_HANDSHAKE_HEADERS: [&str; 4] = [
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Status of a successful protocol switch.
const SWITCHING_PROTOCOLS: u16 = 101;

/// Request payload handed to [`ProxyEngine::send`].
#[derive(Debug, Default)]
pub struct ProxyRequest {
    /// HTTP method of the client request.
    pub method: Method,
    /// Outbound path (already route-rewritten), without the query string.
    pub path: String,
    /// Raw query string, without the leading `?`; may be empty.
    pub query: String,
    /// Client headers, already plugin-transformed.
    pub headers: HeaderMap,
    /// Headers the plugin chain injected into the outbound request, in
    /// injection order (ADR-0002 credential injection, request-id propagation).
    ///
    /// They are applied *after* the client passthrough filter, so an injected
    /// credential replaces a client-supplied header of the same name and is
    /// never dropped by `passthrough: none`.
    pub injected_headers: Vec<(HeaderName, HeaderValue)>,
    /// Query parameters the plugin chain injected, in injection order.
    ///
    /// They override a client parameter of the same name and are appended to
    /// the surviving client parameters, URL-encoded.
    pub injected_query: Vec<(String, String)>,
    /// Client body.
    pub body: ProxyBody,
    /// Optional `x-oagw-target-host` value, already trimmed.
    pub target_host: Option<String>,
    /// `true` when the client asked for a protocol upgrade and the
    /// `Connection`/`Upgrade` headers must survive the pipeline.
    pub upgrade: bool,
    /// Upgrade handle of the *client* socket, lifted out of the inbound
    /// request; the engine splices it with the upstream socket on a `101`.
    pub downstream_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

/// Client body of a [`ProxyRequest`].
#[derive(Debug, Default)]
pub enum ProxyBody {
    /// No body at all.
    #[default]
    Empty,
    /// Body already validated and buffered.
    Buffered(Bytes),
    /// Body still streaming from the client.
    Stream(axum::body::Body),
}

/// Upstream answer handed back to the handler.
#[derive(Debug)]
pub struct ProxyResponse {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Upstream headers, after the response header rules.
    pub headers: HeaderMap,
    /// Upstream body, streamed.
    pub body: axum::body::Body,
    /// `true` when the upstream switched protocols; the body channel is the
    /// spliced tunnel rather than an HTTP body.
    pub upgraded: bool,
}

/// How [`ProxyEngine::select_endpoint`] chose the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// The client pinned the endpoint with `x-oagw-target-host`.
    ExplicitHeader,
    /// The pool has several distinct hosts and no pin; round-robin decided.
    RoundRobin,
    /// The pool has exactly one endpoint.
    Default,
}

impl SelectionMethod {
    /// The label value used by `oagw_routing_endpoint_selected`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// An endpoint chosen by [`ProxyEngine::select_endpoint`].
#[derive(Debug, Clone)]
pub struct EndpointSelection {
    /// The endpoint to call.
    pub endpoint: Endpoint,
    /// How it was chosen.
    pub method: SelectionMethod,
}

/// Outbound transport engine.
pub struct ProxyEngine {
    connector: Arc<TransportConnector>,
    timeout: std::time::Duration,
    ssrf: SsrfPolicy,
    metrics: Arc<MetricsRegistry>,
    cursors: DashMap<Uuid, Arc<AtomicUsize>>,
}

impl std::fmt::Debug for ProxyEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyEngine")
            .field("timeout", &self.timeout)
            .field("ssrf", &self.ssrf)
            .finish_non_exhaustive()
    }
}

impl ProxyEngine {
    /// Builds an engine over a fresh shared transport connector.
    ///
    /// `metrics` receives the routing and duration instruments.
    #[must_use]
    pub fn new(config: &OagwConfig, metrics: Arc<MetricsRegistry>) -> Self {
        let connector = Arc::new(TransportConnector::new(Some(ConnectorOptions::new(128))));
        Self::with_connector(config, connector, metrics)
    }

    /// Builds an engine over an existing connector (tests, custom transports).
    #[must_use]
    pub fn with_connector(
        config: &OagwConfig,
        connector: Arc<TransportConnector>,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        Self {
            connector,
            timeout: config.proxy_timeout(),
            ssrf: config.ssrf_policy.clone(),
            metrics,
            cursors: DashMap::new(),
        }
    }

    /// The shared transport connector (ADR-0006).
    #[must_use]
    pub fn connector(&self) -> &Arc<TransportConnector> {
        &self.connector
    }

    /// The proxy budget every upstream phase is bounded by
    /// (`proxy_timeout_secs`).
    ///
    /// Exposed for the caller to bound its own pre-upstream waits — a `queue`
    /// rate-limit strategy must never spend longer waiting for a token than the
    /// upstream would have allowed in the first place.
    #[must_use]
    pub const fn proxy_timeout(&self) -> std::time::Duration {
        self.timeout
    }

    /// Selects the endpoint to call (ADR-0001).
    ///
    /// # Errors
    ///
    /// * [`OagwError::InvalidTargetHost`] — the pinned host is malformed;
    /// * [`OagwError::UnknownTargetHost`] — the pinned host is not in the pool;
    /// * [`OagwError::MissingTargetHost`] — the pool has several common-suffix
    ///   alias endpoints and the client pinned none;
    /// * [`OagwError::LinkUnavailable`] — the pool is empty.
    pub fn select_endpoint(
        &self,
        upstream: &Upstream,
        requested: Option<&str>,
    ) -> Result<EndpointSelection, OagwError> {
        let endpoints = upstream.server.endpoints.as_slice();
        if endpoints.is_empty() {
            return Err(empty_pool(upstream));
        }
        let hosts: Vec<String> = endpoints
            .iter()
            .map(|endpoint| endpoint.host.to_ascii_lowercase())
            .collect();

        if let Some(pinned) = requested.map(str::trim).filter(|value| !value.is_empty()) {
            let normalized = normalize_requested_host(pinned)?;
            return match hosts
                .iter()
                .position(|host| host == &normalized || strip_trailing_dot(host) == normalized)
            {
                Some(index) => {
                    self.record_selection(
                        upstream,
                        &endpoints[index],
                        SelectionMethod::ExplicitHeader,
                    );
                    Ok(EndpointSelection {
                        endpoint: endpoints[index].clone(),
                        method: SelectionMethod::ExplicitHeader,
                    })
                }
                None => Err(OagwError::unknown_target_host(format!(
                    "target host '{pinned}' does not match any endpoint of upstream '{}'",
                    upstream.alias
                ))
                .with_upstream_id(upstream.id)
                .with_host(pinned)
                .with_valid_hosts(hosts.clone())),
            };
        }

        if endpoints.len() == 1 {
            self.record_selection(upstream, &endpoints[0], SelectionMethod::Default);
            return Ok(EndpointSelection {
                endpoint: endpoints[0].clone(),
                method: SelectionMethod::Default,
            });
        }

        if let Some(alias) = common_suffix_alias(&upstream.alias, &hosts) {
            return Err(OagwError::missing_target_host(format!(
                "upstream '{}' balances the '{alias}' endpoint alias; set {} to one of them",
                upstream.alias, TARGET_HOST_HEADER
            ))
            .with_upstream_id(upstream.id)
            .with_valid_hosts(hosts.clone()));
        }

        let endpoint = self.round_robin(upstream.id, endpoints);
        self.record_selection(upstream, &endpoint, SelectionMethod::RoundRobin);
        Ok(EndpointSelection {
            endpoint,
            method: SelectionMethod::RoundRobin,
        })
    }

    /// Bumps the routing metrics of a selection.
    fn record_selection(&self, upstream: &Upstream, endpoint: &Endpoint, method: SelectionMethod) {
        let upstream_id = upstream.id.to_string();
        self.metrics
            .record_target_host_used(&upstream_id, &endpoint.host);
        self.metrics
            .record_endpoint_selected(&upstream_id, &endpoint.host, method.as_str());
        self.metrics.set_upstream_available(
            &upstream.alias,
            &Self::render_endpoint(endpoint),
            true,
        );
    }

    /// Renders an endpoint as the `endpoint` label value of the availability
    /// gauge, and the `endpoint` field of the proxy audit record.
    pub fn render_endpoint(endpoint: &Endpoint) -> String {
        let scheme = match endpoint.scheme {
            Scheme::Https => "https",
            Scheme::Http => "http",
            Scheme::Wss => "wss",
            Scheme::Ws => "ws",
            Scheme::Wt => "wt",
            Scheme::Grpc => "grpc",
        };
        format!("{scheme}://{}:{}", endpoint.host, endpoint.port)
    }

    /// Advances the per-upstream round-robin cursor.
    fn round_robin(&self, upstream_id: Uuid, endpoints: &[Endpoint]) -> Endpoint {
        let cursor = self
            .cursors
            .entry(upstream_id)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        let index = cursor.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        endpoints[index].clone()
    }

    /// Resolves `host` and applies the SSRF gate.
    ///
    /// # Errors
    ///
    /// [`OagwError::LinkUnavailable`] when the name does not resolve, and
    /// [`OagwError::InvalidTargetHost`] when the resolved address is rejected
    /// by `ssrf_policy`.
    pub async fn resolve_host(&self, host: &str, port: u16) -> Result<IpAddr, OagwError> {
        let mut resolved = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| {
                OagwError::link_unavailable(format!(
                    "upstream host '{host}' does not resolve: {error}"
                ))
            })?;
        let Some(address) = resolved.next() else {
            return Err(OagwError::link_unavailable(format!(
                "upstream host '{host}' does not resolve to any address"
            )));
        };
        if self.ssrf.enabled {
            check_ssrf(&self.ssrf, address.ip())?;
        }
        Ok(address.ip())
    }

    /// Sends `request` to `target` and returns the upstream answer.
    ///
    /// The header pipeline runs in the documented order: strip hop-by-hop,
    /// apply the passthrough mode of the merged upstream → route request
    /// rules, insert the plugin-injected headers over the survivors, apply
    /// those rules (`set`/`add`/`remove`), then set `Host` to the endpoint
    /// authority. The outbound query is the client query with the
    /// plugin-injected parameters appended and overriding their client
    /// counterparts.
    ///
    /// The response body is never buffered: it streams from the upstream
    /// connection into the client response.
    ///
    /// # Errors
    ///
    /// Maps transport failures onto the taxonomy:
    /// [`OagwError::ConnectionTimeout`] for the connect phase,
    /// [`OagwError::RequestTimeout`] for the header exchange,
    /// [`OagwError::LinkUnavailable`] for refused or unresolvable peers,
    /// [`OagwError::ProtocolError`] for TLS and handshake failures and
    /// [`OagwError::StreamAborted`] for a body that dies mid-flight.
    pub async fn send(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        target: &Endpoint,
        request: ProxyRequest,
    ) -> Result<ProxyResponse, OagwError> {
        let started = Instant::now();
        // The duration metrics are keyed on the route *pattern*, never on the
        // outbound path: the latter carries the client's path suffix and would
        // mint one series per resource (see `domain::metrics::route_label`).
        let path = crate::domain::metrics::route_label(
            request.method.as_str(),
            route
                .and_then(|route| route.r#match.http.as_ref())
                .map(|http| http.path.as_str()),
        );
        let ip = self.resolve_host(&target.host, target.port).await?;
        let peer = build_peer(ip, target, self.timeout);

        let connect = self.connector.get_stream(&peer);
        let (stream, _reused) = match tokio::time::timeout(self.timeout, connect).await {
            Ok(result) => result.map_err(|error| map_connect_error(&error, upstream, target))?,
            Err(_) => {
                return Err(OagwError::connection_timeout(format!(
                    "connecting to {}:{} exceeded the proxy budget",
                    target.host, target.port
                ))
                .with_upstream_id(upstream.id)
                .with_host(&target.host));
            }
        };
        self.metrics.record_duration(
            &upstream.alias,
            &path,
            crate::domain::metrics::PHASE_CONNECT,
            started.elapsed().as_secs_f64(),
        );

        let mut request = request;
        let downstream_upgrade = request.downstream_upgrade.take();
        let mut outbound = self.build_outbound_request(upstream, route, request)?;
        apply_header_rules(
            outbound.headers_mut(),
            &effective_header_rules(upstream, route),
        )?;
        set_host(outbound.headers_mut(), &authority(target))?;

        let io = TokioIo::new(stream);
        let handshake = hyper::client::conn::http1::Builder::new().handshake(io);
        let (mut sender, connection) = match tokio::time::timeout(self.timeout, handshake).await {
            Ok(result) => result.map_err(|error| map_exchange_error(&error, upstream, target))?,
            Err(_) => {
                return Err(OagwError::request_timeout(format!(
                    "upstream handshake with {}:{} exceeded the proxy budget",
                    target.host, target.port
                ))
                .with_upstream_id(upstream.id)
                .with_host(&target.host));
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                tracing::debug!("upstream connection finished: {error}");
            }
        });

        let exchange = tokio::time::timeout(self.timeout, sender.send_request(outbound));
        let mut response = match exchange.await {
            Ok(result) => result.map_err(|error| map_exchange_error(&error, upstream, target))?,
            Err(_) => {
                return Err(OagwError::request_timeout(format!(
                    "upstream {}:{} did not answer within the proxy budget",
                    target.host, target.port
                ))
                .with_upstream_id(upstream.id)
                .with_host(&target.host));
            }
        };
        self.metrics.record_duration(
            &upstream.alias,
            &path,
            crate::domain::metrics::PHASE_UPSTREAM,
            started.elapsed().as_secs_f64(),
        );

        // The upgrade handle must be lifted out *before* the response is
        // deconstructed, so the 101 branch reads it from `hyper` directly.
        let upgraded = response.status().as_u16() == SWITCHING_PROTOCOLS;
        let upstream_upgrade = upgraded.then(|| hyper::upgrade::on(&mut response));
        let (parts, incoming) = response.into_parts();
        let body = if upgraded {
            self.splice_upgrade(upstream_upgrade, downstream_upgrade, upstream, target)
                .await?;
            axum::body::Body::empty()
        } else {
            let metrics = Arc::clone(&self.metrics);
            let host = upstream.alias.clone();
            let route_label = path.clone();
            let watched = axum::body::Body::new(incoming)
                .into_data_stream()
                .map(move |frame| {
                    if frame.is_err() {
                        metrics.record_error(
                            &host,
                            &route_label,
                            &OagwError::stream_aborted("upstream stream aborted").gts_type(),
                        );
                    }
                    frame
                });
            axum::body::Body::from_stream(watched)
        };

        Ok(ProxyResponse {
            status: parts.status,
            headers: parts.headers,
            body,
            upgraded,
        })
    }

    /// Joins the two upgraded sockets of a `101 Switching Protocols` exchange.
    ///
    /// The upstream half comes from the exchanged response, the downstream half
    /// from the `OnUpgrade` handle the caller lifted out of the inbound
    /// request; a non-101 answer never reaches this point, so the caller maps
    /// an unexpected status to a `502` before calling.
    ///
    /// # Errors
    ///
    /// [`OagwError::ProtocolError`] when either half refuses the upgrade.
    async fn splice_upgrade(
        &self,
        upstream_upgrade: Option<hyper::upgrade::OnUpgrade>,
        downstream: Option<hyper::upgrade::OnUpgrade>,
        upstream: &Upstream,
        target: &Endpoint,
    ) -> Result<(), OagwError> {
        let (Some(upstream_upgrade), Some(downstream)) = (upstream_upgrade, downstream) else {
            // No client socket to splice: the caller asked for an upgrade it
            // cannot carry, so the answer is a `502` rather than a socket
            // nobody reads.
            return Err(OagwError::protocol_error(format!(
                "upstream {}:{} switched protocols but the caller did not request an upgrade",
                target.host, target.port
            ))
            .with_upstream_id(upstream.id)
            .with_host(&target.host));
        };
        let upstream_io = upstream_upgrade.await.map_err(|error| {
            OagwError::protocol_error(format!(
                "upstream {}:{} did not complete the protocol switch: {error}",
                target.host, target.port
            ))
            .with_upstream_id(upstream.id)
            .with_host(&target.host)
        })?;
        let upstream_id = upstream.id;
        let host = target.host.clone();
        tokio::spawn(async move {
            let mut downstream_io = match downstream.await {
                Ok(io) => TokioIo::new(io),
                Err(error) => {
                    tracing::debug!("downstream upgrade failed: {error}");
                    return;
                }
            };
            let mut server_io = TokioIo::new(upstream_io);
            match tokio::io::copy_bidirectional(&mut downstream_io, &mut server_io).await {
                Ok((to_upstream, from_upstream)) => {
                    tracing::trace!(
                        "upgraded channel to {host} for {upstream_id}: {to_upstream} up, \
                         {from_upstream} down"
                    );
                }
                Err(error) => {
                    tracing::debug!("upgraded channel to {host} for {upstream_id} closed: {error}");
                }
            }
        });
        Ok(())
    }

    /// Builds the outbound HTTP/1.1 request body and URI; the header map is
    /// completed by the caller.
    ///
    /// The header pipeline is: strip hop-by-hop from the client headers, apply
    /// the passthrough mode, then `insert` the plugin-injected headers over the
    /// survivors. The injected set is written last, so a credential a plugin
    /// resolved replaces a client-supplied header of the same name and survives
    /// a `passthrough: none` posture that drops every other client header. The
    /// `Connection`/`Upgrade` pair of a protocol switch is inserted after them,
    /// which is also the correct order: an injected hop-by-hop header
    /// (`connection`, `te`, …) is a plugin misconfiguration and is forwarded
    /// rather than silently discarded.
    fn build_outbound_request(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        request: ProxyRequest,
    ) -> Result<hyper::Request<axum::body::Body>, OagwError> {
        let mut headers = passthrough_filter(
            &strip_hop_by_hop(&request.headers),
            &effective_header_rules(upstream, route),
        );
        for (name, value) in request.injected_headers {
            headers.insert(name, value);
        }
        if request.upgrade {
            headers.insert(
                http::header::CONNECTION,
                HeaderValue::from_static("Upgrade"),
            );
            headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
            // The handshake parameters are not negotiable: a WebSocket server
            // answers 400 without the client's `Sec-WebSocket-Key`, and a
            // sub-protocol negotiated through the gateway must still be the
            // one the client offered. They are lifted out of the client set,
            // so the passthrough mode never hides them.
            for name in WEBSOCKET_HANDSHAKE_HEADERS {
                let Some(value) = request.headers.get(name) else {
                    continue;
                };
                headers.insert(HeaderName::from_static(name), value.clone());
            }
        }

        let query = merge_query(&request.query, &request.injected_query);
        let path_and_query = if query.is_empty() {
            request.path.clone()
        } else {
            format!("{}?{query}", request.path)
        };
        let uri = Uri::builder()
            .path_and_query(path_and_query)
            .build()
            .map_err(|error| OagwError::validation(format!("invalid outbound path: {error}")))?;

        let body = match request.body {
            ProxyBody::Empty => {
                // Nothing is forwarded, so a declared length must not survive:
                // `hyper` sizes the empty body itself.
                headers.remove(http::header::CONTENT_LENGTH);
                axum::body::Body::empty()
            }
            ProxyBody::Buffered(bytes) => {
                // The buffer is the truth about the outbound length: a transform
                // plugin that rewrote the payload leaves the *client's* declared
                // `content-length` behind, and a stale short value makes the
                // upstream truncate the body (or hang waiting for the missing
                // bytes). The streaming arm is left alone — `hyper` renders it
                // chunked, where no length is declared.
                headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
                axum::body::Body::from(bytes)
            }
            ProxyBody::Stream(stream) => stream,
        };

        let mut builder = hyper::Request::builder()
            .method(request.method)
            .version(Version::HTTP_11)
            .uri(uri);
        *builder
            .headers_mut()
            .ok_or_else(|| OagwError::validation("outbound request could not carry headers"))? =
            headers;
        builder
            .body(body)
            .map_err(|error| OagwError::validation(format!("invalid outbound request: {error}")))
    }

    /// Applies the response-side header pipeline to an upstream answer.
    ///
    /// Hop-by-hop headers are stripped, then the upstream response rules and
    /// finally the route response rules run, so a route can override an
    /// upstream value.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`] when a configured header name or value is not
    /// a valid HTTP token.
    pub fn prepare_response(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        mut response: ProxyResponse,
    ) -> Result<ProxyResponse, OagwError> {
        // A switched protocol answer must keep `Connection` and `Upgrade`, or
        // the caller could not tell the socket apart from a plain body; the
        // hop-by-hop stripping only applies to ordinary exchanges.
        let mut headers = if response.upgraded {
            response.headers.clone()
        } else {
            strip_hop_by_hop(&response.headers)
        };
        apply_header_rules(&mut headers, &upstream.headers.response)?;
        if let Some(route) = route {
            apply_header_rules(&mut headers, &route.headers.response)?;
        }
        headers.remove(TARGET_HOST_HEADER);
        response.headers = headers;
        Ok(response)
    }
}

/// Builds the `503 LinkUnavailable` problem for an endpoint-less upstream.
fn empty_pool(upstream: &Upstream) -> OagwError {
    OagwError::link_unavailable(format!("upstream '{}' has no endpoint", upstream.alias))
        .with_upstream_id(upstream.id)
}

/// Overwrites the `Host` header with the endpoint authority.
fn set_host(headers: &mut HeaderMap, host: &str) -> Result<(), OagwError> {
    headers.insert(http::header::HOST, header_value(host)?);
    Ok(())
}

/// The `Host`/`:authority` value of `target`: the endpoint host, carrying the
/// port whenever the endpoint does not use its scheme default.
#[must_use]
fn authority(target: &Endpoint) -> String {
    let default = if is_tls(target.scheme) { 443 } else { 80 };
    if target.port == default {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    }
}

/// Builds the pingora peer for `ip`/`target`.
fn build_peer(ip: IpAddr, target: &Endpoint, timeout: std::time::Duration) -> HttpPeer {
    let tls = is_tls(target.scheme);
    let mut peer = HttpPeer::new(SocketAddr::new(ip, target.port), tls, target.host.clone());
    peer.options.connection_timeout = Some(timeout);
    peer.options.total_connection_timeout = Some(timeout);
    peer.options.read_timeout = Some(timeout);
    peer.options.write_timeout = Some(timeout);
    peer.options.idle_timeout = Some(timeout);
    peer
}

/// `true` when the endpoint scheme speaks HTTP over TLS.
#[must_use]
pub const fn is_tls(scheme: Scheme) -> bool {
    matches!(scheme, Scheme::Https | Scheme::Wss | Scheme::Grpc)
}

/// Maps a pingora connect failure onto the taxonomy.
fn map_connect_error(
    error: &pingora_core::BError,
    upstream: &Upstream,
    target: &Endpoint,
) -> OagwError {
    let etype = &error.etype;
    let detail = format!(
        "upstream {}:{} failed: {}",
        target.host,
        target.port,
        error.context.as_ref().map_or_else(
            || etype.as_str().to_owned(),
            |context| format!("{}: {context}", etype.as_str())
        )
    );
    let base = match etype {
        pingora_core::ErrorType::ConnectTimedout
        | pingora_core::ErrorType::TLSHandshakeTimedout => OagwError::connection_timeout(detail),
        pingora_core::ErrorType::TLSHandshakeFailure
        | pingora_core::ErrorType::InvalidCert
        | pingora_core::ErrorType::HandshakeError => OagwError::protocol_error(detail),
        _ => OagwError::link_unavailable(detail),
    };
    enrich(base, upstream, target)
}

/// Maps a hyper protocol failure onto the taxonomy.
fn map_exchange_error(error: &hyper::Error, upstream: &Upstream, target: &Endpoint) -> OagwError {
    enrich(
        classify_exchange(
            error.is_timeout(),
            error.is_body_write_aborted(),
            error.is_incomplete_message(),
            target,
        ),
        upstream,
        target,
    )
}

/// Classifies one hyper exchange failure from the three flags the type
/// exposes, so the mapping stays testable without hyper's private
/// constructors.
fn classify_exchange(
    timeout: bool,
    body_write_aborted: bool,
    incomplete_message: bool,
    target: &Endpoint,
) -> OagwError {
    let detail = format!("upstream {}:{} failed", target.host, target.port);
    if timeout {
        OagwError::request_timeout(detail)
    } else if body_write_aborted || incomplete_message {
        OagwError::stream_aborted(detail)
    } else {
        OagwError::protocol_error(detail)
    }
}

/// Stamps the upstream identity onto an error.
fn enrich(error: OagwError, upstream: &Upstream, target: &Endpoint) -> OagwError {
    error.with_upstream_id(upstream.id).with_host(&target.host)
}

/// Normalises and validates a client-pinned target host.
///
/// # Errors
///
/// [`OagwError::InvalidTargetHost`] when the value is not a syntactically
/// valid host name or IP literal.
fn normalize_requested_host(pinned: &str) -> Result<String, OagwError> {
    let host = strip_port(pinned);
    if let Err(reason) = classify_host(host) {
        return Err(OagwError::invalid_target_host(format!(
            "target host '{pinned}' is not a valid endpoint host: {reason}"
        ))
        .with_invalid_value(pinned));
    }
    Ok(host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase())
}

/// Removes a trailing `:port` or bracketed port from a host header value.
fn strip_port(value: &str) -> &str {
    if let Some(rest) = value.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
        && (tail.is_empty() || tail.starts_with(':'))
    {
        return host;
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => value,
    }
}

/// Drops the trailing dot of a fully qualified host name.
fn strip_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// Detects the ADR-0001 "common-suffix alias" pools.
///
/// The alias of a hostname-based endpoint pool *is* its registrable common
/// suffix (DESIGN section 3.1), so an upstream named `example.com` balancing
/// `payments-eu.example.com` and `payments-us.example.com` does not name a
/// single upstream target: the client has to pin one with
/// [`TARGET_HOST_HEADER`]. Returns `Some(alias)` when every host of the pool is
/// that alias or ends with `.` + that alias and at least one host is longer.
#[must_use]
fn common_suffix_alias(alias: &str, hosts: &[String]) -> Option<String> {
    if hosts.len() < 2 {
        return None;
    }
    let alias = alias.trim().to_ascii_lowercase();
    if alias.is_empty() {
        return None;
    }
    let all_match = hosts.iter().all(|host| {
        let host = strip_trailing_dot(host);
        host == alias || host.strip_suffix(&format!(".{alias}")).is_some()
    });
    let some_longer = hosts
        .iter()
        .any(|host| strip_trailing_dot(host).len() > alias.len());
    if all_match && some_longer {
        Some(alias)
    } else {
        None
    }
}

/// Detects a WebSocket upgrade request.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection = header_token_is(headers.get(http::header::CONNECTION), "upgrade");
    let upgrade = header_token_is(headers.get(http::header::UPGRADE), "websocket");
    connection && upgrade
}

/// `true` when the comma-separated header value contains `expected`.
fn header_token_is(value: Option<&HeaderValue>, expected: &str) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .split(',')
                .any(|token| token.trim() == expected)
        })
}

/// Strips the hop-by-hop headers and the routing header from `headers`.
#[must_use]
pub fn strip_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    let mut stripped = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let key = name.as_str();
        if HOP_BY_HOP.contains(&key) || CONSUMED.contains(&key) {
            continue;
        }
        stripped.insert(name.clone(), value.clone());
    }
    stripped
}

/// Merges the request or response header rules of the upstream → route chain
/// (DESIGN "Hierarchical Configuration": the route is the more specific tier
/// and wins).
///
/// * `set` — a route entry replaces an upstream entry with the same name;
/// * `add` — both sets are appended, upstream first;
/// * `remove` — the union of both name lists;
/// * `passthrough` — the route value when it is not the default `none`,
///   otherwise the upstream value;
/// * `passthrough_allowlist` — the union of both lists.
#[must_use]
pub fn effective_header_rules(upstream: &Upstream, route: Option<&Route>) -> HeaderRules {
    let parent = &upstream.headers.request;
    let mut merged = HeaderRules {
        set: parent.set.clone(),
        add: parent.add.clone(),
        remove: parent.remove.clone(),
        passthrough: parent.passthrough,
        passthrough_allowlist: parent.passthrough_allowlist.clone(),
    };
    let Some(route) = route else {
        return merged;
    };
    let child = &route.headers.request;
    for (name, value) in &child.set {
        merged.set.insert(name.clone(), value.clone());
    }
    for (name, value) in &child.add {
        merged.add.insert(name.clone(), value.clone());
    }
    for name in &child.remove {
        if !merged.remove.contains(name) {
            merged.remove.push(name.clone());
        }
    }
    if !child.passthrough.is_none() {
        merged.passthrough = child.passthrough;
    }
    for name in &child.passthrough_allowlist {
        if !merged
            .passthrough_allowlist
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(name))
        {
            merged.passthrough_allowlist.push(name.clone());
        }
    }
    merged
}

/// Applies the `passthrough` policy to the client header map.
///
/// The gateway-owned transport headers (`content-type`, `content-length`) are
/// always preserved so the exchange stays a well-formed HTTP/1.1 request; the
/// `Host` header is set separately by [`ProxyEngine::send`].
#[must_use]
pub fn passthrough_filter(headers: &HeaderMap, rules: &HeaderRules) -> HeaderMap {
    let mut filtered = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let key = name.as_str();
        let owned = OWNED.contains(&key);
        let allowed = match rules.passthrough {
            PassthroughMode::All => true,
            PassthroughMode::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(key)),
            PassthroughMode::None => false,
        };
        if owned || allowed {
            filtered.insert(name.clone(), value.clone());
        }
    }
    filtered
}

/// Applies the `set`/`add`/`remove` header rules of one direction.
///
/// # Errors
///
/// [`OagwError::Validation`] when a configured header name or value is not a
/// valid HTTP token.
pub fn apply_header_rules(headers: &mut HeaderMap, rules: &HeaderRules) -> Result<(), OagwError> {
    for (name, value) in &rules.set {
        headers.insert(header_name(name)?, header_value(value)?);
    }
    for (name, value) in &rules.add {
        headers.append(header_name(name)?, header_value(value)?);
    }
    for name in &rules.remove {
        headers.remove(header_name(name)?);
    }
    Ok(())
}

/// Parses a header name from configuration.
///
/// # Errors
///
/// [`OagwError::Validation`] when the name is not a valid HTTP token.
pub fn header_name(name: &str) -> Result<HeaderName, OagwError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| OagwError::validation(format!("invalid header name '{name}': {error}")))
}

/// Parses a header value from configuration.
///
/// # Errors
///
/// [`OagwError::Validation`] when the value contains invalid bytes.
pub fn header_value(value: &str) -> Result<HeaderValue, OagwError> {
    HeaderValue::from_str(value)
        .map_err(|error| OagwError::validation(format!("invalid header value: {error}")))
}

/// Builds the outbound query string from the client query and the
/// plugin-injected parameters (PRD §5.2 credential injection).
///
/// The client part is copied **verbatim** — a gateway that injects nothing
/// never rewrites a caller's query, so `?a=1&b=%2Ftwo` stays exactly that. The
/// injected pairs are appended after the surviving client pairs, in injection
/// order, `application/x-www-form-urlencoded`-encoded by [`form_urlencoded`].
///
/// A client parameter of an injected name is dropped rather than duplicated:
/// an injected credential must not leak alongside a caller-supplied one, and a
/// duplicated name would leave the upstream to guess which value counts. The
/// comparison decodes the client name, so `api%5Fkey=spoofed` is overridden by
/// an injected `api_key` too.
#[must_use]
pub fn merge_query(client: &str, injected: &[(String, String)]) -> String {
    let client = client.trim_start_matches('?');
    if injected.is_empty() {
        return client.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    for segment in client.split('&') {
        if segment.is_empty() {
            continue;
        }
        let raw_name = segment.split('=').next().unwrap_or(segment);
        let overridden = form_urlencoded::parse(raw_name.as_bytes())
            .next()
            .is_some_and(|(name, _)| injected.iter().any(|(key, _)| *key == name));
        if !overridden {
            kept.push(segment);
        }
    }
    let mut query = kept.join("&");
    for (name, value) in injected {
        let pair = form_urlencoded::Serializer::new(String::new())
            .append_pair(name, value)
            .finish();
        if query.is_empty() {
            query = pair;
        } else {
            query.push('&');
            query.push_str(&pair);
        }
    }
    query
}

/// Validates the declared and actual request body size before forwarding.
///
/// # Errors
///
/// * [`OagwError::Validation`] — `Content-Length` is not an integer or
///   disagrees with the actual body size;
/// * [`OagwError::PayloadTooLarge`] — the body exceeds the 100 MiB cap;
/// * [`OagwError::Validation`] — `Transfer-Encoding` is present but not
///   `chunked`.
pub fn validate_body(headers: &HeaderMap, body: Option<&[u8]>) -> Result<usize, OagwError> {
    validate_transfer_encoding(headers)?;
    let actual = body.map_or(0, <[u8]>::len);
    if actual > MAX_REQUEST_BODY_BYTES {
        return Err(OagwError::payload_too_large(format!(
            "request body exceeds the {MAX_REQUEST_BODY_BYTES} byte cap"
        )));
    }
    if let Some(parsed) = declared_content_length(headers)? {
        if parsed > MAX_REQUEST_BODY_BYTES {
            // A declared body larger than the cap is rejected before the
            // content is compared, so the client learns it is a size problem
            // and not a mismatch.
            return Err(OagwError::payload_too_large(format!(
                "declared Content-Length {parsed} exceeds the {MAX_REQUEST_BODY_BYTES} byte cap"
            )));
        }
        if parsed != actual {
            return Err(OagwError::validation(format!(
                "Content-Length {parsed} does not match the actual body size {actual}"
            )));
        }
    }
    Ok(actual)
}

/// Rejects a declared `Content-Length` over the 100 MiB cap *before* the body
/// is read (DESIGN "Body Validation Rules": "Hard limit 100MB … reject before
/// buffering").
///
/// # Errors
///
/// * [`OagwError::PayloadTooLarge`] — the declared size exceeds the cap;
/// * [`OagwError::Validation`] — the header is not ASCII or not an integer.
pub fn validate_declared_body_size(headers: &HeaderMap) -> Result<(), OagwError> {
    if declared_content_length(headers)?.is_some_and(|parsed| parsed > MAX_REQUEST_BODY_BYTES) {
        return Err(OagwError::payload_too_large(format!(
            "declared Content-Length exceeds the {MAX_REQUEST_BODY_BYTES} byte cap"
        )));
    }
    Ok(())
}

/// Parses the declared `Content-Length`, if the request carries one.
fn declared_content_length(headers: &HeaderMap) -> Result<Option<usize>, OagwError> {
    let Some(declared) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let text = declared
        .to_str()
        .map_err(|_| OagwError::validation("Content-Length is not ASCII"))?
        .trim();
    let parsed: usize = text.parse().map_err(|_| {
        OagwError::validation(format!("Content-Length '{text}' is not a valid integer"))
    })?;
    Ok(Some(parsed))
}

/// Validates the `Transfer-Encoding` header, which only admits `chunked`.
fn validate_transfer_encoding(headers: &HeaderMap) -> Result<(), OagwError> {
    let Some(transfer) = headers.get(http::header::TRANSFER_ENCODING) else {
        return Ok(());
    };
    let value = transfer
        .to_str()
        .map_err(|_| OagwError::validation("Transfer-Encoding is not ASCII"))?
        .to_ascii_lowercase();
    let tokens: Vec<&str> = value.split(',').map(str::trim).collect();
    if tokens.contains(&"identity") {
        return Err(OagwError::validation(
            "Transfer-Encoding 'identity' is not allowed; use 'chunked'",
        ));
    }
    if !tokens.contains(&"chunked") {
        return Err(OagwError::validation("Transfer-Encoding must be 'chunked'"));
    }
    Ok(())
}

/// Applies the SSRF gate to a resolved address.
///
/// An IPv4-mapped IPv6 literal (`::ffff:127.0.0.1`) is classified as the IPv4
/// address it carries: taken at face value it looks like global unicast IPv6
/// and a private-range IPv4 host would walk straight past the gate. The
/// normalised address is also the one the `allowed_ip_ranges` check and the
/// problem document name.
///
/// # Errors
///
/// [`OagwError::InvalidTargetHost`] when the address is loopback, private,
/// link-local or unspecified and the policy forbids it, or when it is not in
/// `allowed_ip_ranges`.
fn check_ssrf(policy: &SsrfPolicy, address: IpAddr) -> Result<(), OagwError> {
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        plain => plain,
    };
    let blocked = match address {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
        }
    };
    if blocked && !policy.allow_private_networks {
        return Err(OagwError::invalid_target_host(format!(
            "upstream address {address} is in a forbidden network range"
        )));
    }
    if !policy.allowed_ip_ranges.is_empty()
        && !policy
            .allowed_ip_ranges
            .iter()
            .any(|range| cidr_contains(range, address))
    {
        return Err(OagwError::invalid_target_host(format!(
            "upstream address {address} is not in ssrf_policy.allowed_ip_ranges"
        )));
    }
    Ok(())
}

/// `true` when `address` is inside the CIDR block `range`.
fn cidr_contains(range: &str, address: IpAddr) -> bool {
    let Some((network, prefix)) = range.trim().split_once('/') else {
        return false;
    };
    let Ok(bits) = prefix.parse::<u32>() else {
        return false;
    };
    let Ok(network) = network.parse::<IpAddr>() else {
        return false;
    };
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => bits <= 32 && prefix4(network, address, bits),
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            bits <= 128 && prefix6(network, address, bits)
        }
        _ => false,
    }
}

/// `true` when `address` shares the leading `bits` of `network`.
fn prefix4(network: Ipv4Addr, address: Ipv4Addr, bits: u32) -> bool {
    if bits == 0 {
        return true;
    }
    let shift = 32 - bits;
    u32::from(network) >> shift == u32::from(address) >> shift
}

/// `true` when `address` shares the leading `bits` of `network`.
fn prefix6(network: Ipv6Addr, address: Ipv6Addr, bits: u32) -> bool {
    if bits == 0 {
        return true;
    }
    let shift = 128 - bits;
    u128::from(network) >> shift == u128::from(address) >> shift
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
