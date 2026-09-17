//! Outbound proxy engine: endpoint selection, header hygiene, and the three
//! transports (buffered HTTP, streaming/SSE, WebSocket upgrade).

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use toolkit_http::{HttpClient, HttpClientBuilder, HttpClientConfig, RateLimitConfig, RetryConfig};
use url::Url;

use crate::config::OagwConfig;
use crate::domain::alias::{is_common_suffix_alias, normalize, validate_host};
use crate::domain::error::{ERROR_SOURCE_UPSTREAM, ErrorKind, OagwError};
use crate::domain::model::{Endpoint, EndpointScheme, HeadersConfig, Passthrough, Upstream};

/// Headers that are never forwarded in either direction (RFC 9110 §7.6.1).
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers always forwarded with a body, even when passthrough is `none`.
const BODY_HEADERS: [&str; 2] = ["content-type", "content-length"];

/// Header the client uses to pin a multi-endpoint upstream to one endpoint.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Whether `name` belongs to the gateway's own transport protocol.
///
/// Every `x-oagw-*` header is consumed by the gateway: the routing header is
/// read during routing and then stripped (`DESIGN.md` §"Header Handling"), and
/// the remaining members describe the relay itself. None of them ever reaches
/// a third-party upstream, whatever the passthrough rules say.
#[must_use]
pub fn is_oagw_header(name: &str) -> bool {
    name.to_ascii_lowercase()
        .starts_with(crate::domain::error::OAGW_HEADER_PREFIX)
}

/// Whether `name` is a hop-by-hop header.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

/// Whether `name` should be forwarded even without an explicit allowlist.
#[must_use]
pub fn is_body_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BODY_HEADERS.contains(&lower.as_str())
}

/// Whether `name` has to travel on a WebSocket upgrade path for the origin to
/// be able to answer `101`.
#[must_use]
pub fn is_upgrade_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "connection" || lower == "upgrade" || lower.starts_with("sec-websocket-")
}

/// Copy the upgrade headers the passthrough filter removed back into `outbound`.
///
/// `connection` and `upgrade` are hop-by-hop and `sec-websocket-*` is not a
/// body header, so with the default `passthrough: none` the filtered set cannot
/// negotiate a protocol switch; the client's own values are restored verbatim
/// for the upgrade relay only.
pub fn restore_upgrade_headers(outbound: &mut HeaderMap, inbound: &HeaderMap) {
    for (name, value) in inbound {
        if !is_upgrade_header(name.as_str()) {
            continue;
        }
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "connection" || lower == "upgrade" {
            outbound.insert(name.clone(), value.clone());
        } else if outbound.get(name).is_none() {
            outbound.append(name.clone(), value.clone());
        }
    }
}

/// An outbound request ready to be dialed.
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// HTTP method.
    pub method: Method,
    /// Absolute upstream URL.
    pub url: Url,
    /// Outbound headers (already filtered and transformed).
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
}

/// The dialable endpoint selected for a proxy request.
#[derive(Debug, Clone)]
pub struct SelectedEndpoint {
    /// Endpoint to dial.
    pub endpoint: Endpoint,
    /// Host value sent as `Host` (endpoint authority).
    pub host: String,
}

/// A relayed upstream response, whichever transport produced it.
///
/// The standard methods come back from the shared toolkit client, the remaining
/// verbs from the generic-method relay; the relay code needs only the status,
/// the headers, and a body it can either buffer or stream on to the caller.
#[derive(Debug)]
pub struct UpstreamResponse {
    status: StatusCode,
    headers: HeaderMap,
    max_body_size: usize,
    body: UpstreamBody,
}

#[derive(Debug)]
enum UpstreamBody {
    /// A shared-client body: decompressed, and bounded by the client's own
    /// response ceiling.
    Client(toolkit_http::ResponseBody),
    /// A body the generic-method relay already buffered in full.
    Inline(Bytes),
}

impl UpstreamResponse {
    fn from_client(response: toolkit_http::HttpResponse) -> Self {
        let max_body_size = response.max_body_size();
        let (parts, body) = response.into_inner().into_parts();
        Self {
            status: parts.status,
            headers: parts.headers,
            max_body_size,
            body: UpstreamBody::Client(body),
        }
    }

    fn inline(status: StatusCode, headers: HeaderMap, body: Bytes) -> Self {
        Self {
            status,
            headers,
            max_body_size: 0,
            body: UpstreamBody::Inline(body),
        }
    }

    /// The upstream status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The upstream headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Buffer the relayed body, bounding every read by `deadline`.
    ///
    /// The client's own timeout bounds the time to the response headers only,
    /// so a stalled body is cut here rather than pinning the request.
    ///
    /// # Errors
    ///
    /// Returns `idle.timeout` when the body stalls, and a bad-gateway error
    /// when the body cannot be read: a ceiling breach is the upstream's
    /// response, never the caller's request.
    pub async fn bytes_within(self, deadline: Duration) -> Result<Bytes, OagwError> {
        match self.body {
            UpstreamBody::Inline(bytes) => Ok(bytes),
            UpstreamBody::Client(body) => {
                let body = axum::body::Body::new(body);
                match tokio::time::timeout(deadline, axum::body::to_bytes(body, self.max_body_size))
                    .await
                {
                    Ok(Ok(bytes)) => Ok(bytes),
                    Ok(Err(error)) => Err(relayed_body_error(error.to_string())),
                    Err(_) => Err(OagwError::new(
                        ErrorKind::IdleTimeout,
                        "upstream response body stalled mid-relay",
                    )),
                }
            }
        }
    }

    /// The relayed body, ready to be streamed on to the caller.
    pub fn into_body(self) -> axum::body::Body {
        match self.body {
            UpstreamBody::Inline(bytes) => axum::body::Body::from(bytes),
            UpstreamBody::Client(body) => axum::body::Body::new(body),
        }
    }
}

/// A relayed body that could not be read: a bad gateway, never the caller's 413.
fn relayed_body_error(detail: impl std::fmt::Display) -> OagwError {
    OagwError::new(
        ErrorKind::DownstreamError,
        format!("upstream response body could not be relayed: {detail}"),
    )
}

/// Bound every frame of a streamed relay by `deadline`.
///
/// The per-read bound `proxy_timeout_secs` documents for streaming responses is
/// enforced here: an upstream that stops writing mid-body ends the relay instead
/// of holding it open. The status line is already on the wire by then, so the
/// caller sees a truncated stream rather than a problem document.
pub fn bounded_frames(
    body: axum::body::Body,
    deadline: Duration,
) -> impl futures_util::Stream<Item = Result<Bytes, axum::Error>> {
    futures_util::stream::unfold(
        (body.into_data_stream(), deadline),
        |(mut stream, deadline)| async move {
            use futures_util::StreamExt;
            match tokio::time::timeout(deadline, stream.next()).await {
                Ok(Some(frame)) => Some((frame, (stream, deadline))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(axum::Error::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream stream stalled",
                    ))),
                    (stream, deadline),
                )),
            }
        },
    )
}

/// Every address `host` resolves to, in resolver order.
///
/// A literal address is returned without consulting the resolver, so an IP
/// endpoint is never judged on a name it does not have.
///
/// # Errors
///
/// Propagates the resolver's failure.
pub async fn resolve_addresses(host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
    if let Ok(literal) = host.parse::<IpAddr>() {
        return Ok(vec![literal]);
    }
    let mut addresses = Vec::new();
    for socket in tokio::net::lookup_host((host, port)).await? {
        addresses.push(socket.ip());
    }
    Ok(addresses)
}

/// The plaintext HTTP client used only for the methods `toolkit-http` cannot
/// name (`PROPFIND`, `TRACE`, and any custom verb a route allows).
type GenericClient = LegacyClient<HttpConnector, axum::body::Body>;

/// Outbound HTTP transport shared by every proxy request.
pub struct ProxyEngine {
    http: HttpClient,
    /// Built lazily: most deployments never send a non-standard method.
    generic: OnceLock<GenericClient>,
    config: OagwConfig,
}

impl ProxyEngine {
    /// Build the engine with the gear's upstream deadline.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying HTTP client cannot be built.
    pub fn new(config: OagwConfig) -> Result<Self, OagwError> {
        // Every inherited default is stated explicitly: a proxy must not apply
        // a policy the caller never asked for.
        let mut client_config = HttpClientConfig::default();
        // A relayed request is the caller's, and may be non-idempotent: it is
        // sent exactly once, never retried.
        client_config.retry = Some(RetryConfig::disabled());
        // The relay is the caller's capacity, not a shared pool with an
        // undocumented ceiling: an inherited concurrency limit would shed load
        // the caller never agreed to shed (`link.unavailable` 503s).
        client_config.rate_limit = Some(RateLimitConfig::unlimited());
        let http = HttpClientBuilder::with_config(client_config)
            .timeout(config.proxy_timeout())
            .total_timeout(config.proxy_timeout())
            .no_redirects()
            // The relayed *response* is bounded by the same ceiling as the
            // caller's request body: the toolkit default (10 MiB) would fail an
            // ordinary large relay as a client-facing 413.
            .max_body_size(config.max_request_body_bytes)
            .build()
            .map_err(crate::domain::error::OagwError::from)?;
        Ok(Self {
            http,
            generic: OnceLock::new(),
            config,
        })
    }

    /// The gear configuration in force.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Whether an endpoint may be dialed under the current policy.
    ///
    /// # Errors
    ///
    /// Returns `link.unavailable` for plaintext endpoints when
    /// `allow_http_upstream` is `false`, and a validation error when the SSRF
    /// policy rejects the host.
    pub async fn check_endpoint(&self, endpoint: &Endpoint) -> Result<(), OagwError> {
        if endpoint.is_plaintext() && !self.config.allow_http_upstream {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                "plaintext upstream endpoints are disabled by allow_http_upstream=false",
            )
            .with_host(&endpoint.host));
        }
        self.check_ssrf(endpoint).await?;
        Ok(())
    }

    /// Apply the SSRF policy to `endpoint`, before anything is dialed.
    ///
    /// # Errors
    ///
    /// Returns `link.unavailable` when the host is denied by the policy.
    pub async fn check_ssrf(&self, endpoint: &Endpoint) -> Result<(), OagwError> {
        let policy = &self.config.ssrf_policy;
        if !policy.enabled {
            return Ok(());
        }
        let host = normalize(&endpoint.host);
        let allowlisted = policy
            .allowed_hosts
            .iter()
            .any(|allowed| normalize(allowed) == host);
        if allowlisted {
            return Ok(());
        }
        if !policy.deny_private_addresses {
            return Ok(());
        }
        // A name is judged on every address it resolves to: a hostname that
        // points at private space is as unreachable-by-policy as the literal.
        // A name that does not resolve at all is left to the dial, which fails
        // with the same unavailable-link error the transport reports.
        let addresses = if let Ok(literal) = endpoint.host.parse::<IpAddr>() {
            vec![literal]
        } else {
            match resolve_addresses(&endpoint.host, endpoint.effective_port()).await {
                Ok(addresses) => addresses,
                Err(_) => return Ok(()),
            }
        };
        if addresses
            .iter()
            .any(|address| crate::domain::alias::is_private_address(*address))
        {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                "upstream host resolves into private address space",
            )
            .with_host(&endpoint.host));
        }
        Ok(())
    }

    /// Build the absolute upstream URL for `endpoint`, `path`, and `query`.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the URL cannot be constructed.
    pub fn build_url(
        endpoint: &Endpoint,
        path: &str,
        query: Option<&str>,
    ) -> Result<Url, OagwError> {
        let mut url = Url::parse(&format!(
            "{}://{}{}",
            endpoint.scheme.prefix(),
            endpoint.authority(),
            path
        ))
        .map_err(|_| {
            OagwError::new(
                ErrorKind::ProtocolError,
                "upstream URL could not be constructed",
            )
        })?;
        url.set_query(query);
        Ok(url)
    }

    /// Send a buffered request and return the upstream response.
    ///
    /// The method is passed through: every method the shared client can name is
    /// sent through it, and anything else is relayed verbatim on the
    /// generic-method transport.
    ///
    /// # Errors
    ///
    /// Returns a gateway error classified per `ADR 0007` when the exchange
    /// fails.
    pub async fn send(&self, request: OutboundRequest) -> Result<UpstreamResponse, OagwError> {
        let url = request.url.as_str().to_owned();
        let builder = match request.method.as_str() {
            "GET" => Some(self.http.get(&url)),
            "POST" => Some(self.http.post(&url)),
            "PUT" => Some(self.http.put(&url)),
            "PATCH" => Some(self.http.patch(&url)),
            "DELETE" => Some(self.http.delete(&url)),
            "HEAD" => Some(self.http.head(&url)),
            "OPTIONS" => Some(self.http.options(&url)),
            _ => None,
        };
        let Some(builder) = builder else {
            return self.send_generic(&request).await;
        };
        let headers: Vec<(String, String)> = request
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|ascii| (name.as_str().to_owned(), ascii.to_owned()))
            })
            .collect();
        let response = builder
            .headers(headers)
            .body_bytes(request.body)
            .send()
            .await
            .map_err(crate::domain::error::OagwError::from)?;
        Ok(UpstreamResponse::from_client(response))
    }

    /// The client for the methods `toolkit-http` cannot name, built on first use.
    fn generic_client(&self) -> &GenericClient {
        self.generic.get_or_init(|| {
            LegacyClient::builder(TokioExecutor::new()).build_http::<axum::body::Body>()
        })
    }

    /// Relay `request` with a method the shared client cannot express.
    ///
    /// `toolkit-http` exposes one builder per standard method, so the relay
    /// falls back to a plain HTTP/1.1 dial that passes the verb through
    /// untouched. Only plaintext upstreams are reachable this way: a secure
    /// endpoint is refused rather than downgraded.
    async fn send_generic(&self, request: &OutboundRequest) -> Result<UpstreamResponse, OagwError> {
        if request.url.scheme() != "http" {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "a {} request cannot be relayed to a secure upstream",
                    request.method.as_str()
                ),
            )
            .with_host(request.url.host_str().unwrap_or_default()));
        }
        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(request.url.as_str())
            .version(http::Version::HTTP_11);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let outgoing = builder
            .body(axum::body::Body::from(request.body.clone()))
            .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))?;
        let response = tokio::time::timeout(
            self.config.proxy_timeout(),
            self.generic_client().request(outgoing),
        )
        .await
        .map_err(|_| OagwError::new(ErrorKind::RequestTimeout, "upstream did not answer in time"))?
        .map_err(|err| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream transport failed: {err}"),
            )
        })?;
        let (parts, body) = response.into_parts();
        let body = tokio::time::timeout(
            self.config.proxy_timeout(),
            axum::body::to_bytes(
                axum::body::Body::new(body),
                self.config.max_request_body_bytes,
            ),
        )
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::IdleTimeout,
                "upstream response body stalled mid-relay",
            )
        })?
        .map_err(relayed_body_error)?;
        Ok(UpstreamResponse::inline(parts.status, parts.headers, body))
    }

    /// Relay a WebSocket upgrade to `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns a gateway error when the upstream refuses the upgrade or when
    /// TLS is required but unavailable.
    pub async fn relay_websocket(
        &self,
        endpoint: &Endpoint,
        outbound: &OutboundRequest,
        mut client_request: http::Request<axum::body::Body>,
    ) -> Result<axum::response::Response, OagwError> {
        self.check_endpoint(endpoint).await?;
        if endpoint.scheme.is_tls() {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                "secure WebSocket relay is unavailable in this build",
            )
            .with_host(&endpoint.host));
        }

        let mut builder = http::Request::builder()
            .method(outbound.method.clone())
            .uri(outbound.url.as_str())
            .version(http::Version::HTTP_11);
        for (name, value) in &outbound.headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(axum::body::Body::empty())
            .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))?;

        let port = endpoint.effective_port();
        let addr = (endpoint.host.as_str(), port);
        let tcp = tokio::time::timeout(
            self.config.proxy_timeout(),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::ConnectionTimeout,
                "upstream connection timed out",
            )
            .with_host(&endpoint.host)
        })?
        .map_err(|_| {
            OagwError::new(ErrorKind::LinkUnavailable, "upstream link unavailable")
                .with_host(&endpoint.host)
        })?;

        let io = hyper_util::rt::TokioIo::new(tcp);
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|err| {
                    OagwError::new(
                        ErrorKind::ProtocolError,
                        format!("upstream handshake failed: {err}"),
                    )
                })?;
        tokio::spawn(async move {
            if let Err(_err) = connection.with_upgrades().await {
                // The upstream connection closed; nothing further to report
                // because the relay task owns the client side.
            }
        });

        let upstream_response =
            tokio::time::timeout(self.config.proxy_timeout(), sender.send_request(request))
                .await
                .map_err(|_| {
                    OagwError::new(ErrorKind::RequestTimeout, "upstream upgrade timed out")
                })?
                .map_err(|err| {
                    OagwError::new(
                        ErrorKind::ProtocolError,
                        format!("upstream upgrade failed: {err}"),
                    )
                })?;

        if upstream_response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                format!(
                    "upstream did not accept the WebSocket upgrade (status {})",
                    upstream_response.status().as_u16()
                ),
            ));
        }

        let mut client_response = axum::response::Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .version(http::Version::HTTP_11);
        for (name, value) in upstream_response.headers() {
            // RFC 6455 §4.2.2: the server's `101` must carry `upgrade`,
            // `connection`, and the `sec-websocket-*` negotiation headers, so
            // the response-direction twin of [`restore_upgrade_headers`] lets
            // them through even though they are hop-by-hop.
            if is_hop_by_hop(name.as_str()) && !is_upgrade_header(name.as_str()) {
                continue;
            }
            client_response = client_response.header(name, value);
        }
        let response = client_response
            .body(axum::body::Body::empty())
            .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))?;

        tokio::spawn(async move {
            let Ok(client_io) = hyper::upgrade::on(&mut client_request).await else {
                return;
            };
            let Ok(upstream_io) = hyper::upgrade::on(upstream_response).await else {
                return;
            };
            let (mut client_io, mut upstream_io) = (
                hyper_util::rt::TokioIo::new(client_io),
                hyper_util::rt::TokioIo::new(upstream_io),
            );
            // The tunnel's whole job is to move bytes until one side hangs up,
            // and a relayed upgrade has no channel left to report the end on, so
            // only the fact that the copy is over matters here.
            if tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io)
                .await
                .is_err()
            {
                // Either peer closed the socket; both are dropped with this task.
            }
        });

        Ok(response)
    }
}

/// Select the endpoint to dial, applying the `X-OAGW-Target-Host` behaviour
/// matrix of `ADR 0001`.
///
/// # Errors
///
/// Returns the three routing errors (`missing_target_host`,
/// `invalid_target_host`, `unknown_target_host`) as applicable.
pub fn select_endpoint(
    upstream: &Upstream,
    target_host: Option<&str>,
    round_robin: u64,
) -> Result<Endpoint, OagwError> {
    let endpoints = &upstream.server.endpoints;
    if endpoints.is_empty() {
        return Err(OagwError::new(
            ErrorKind::LinkUnavailable,
            "upstream has no configured endpoints",
        )
        .with_upstream(upstream.id));
    }
    let requested = target_host
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize);

    if let Some(requested) = requested {
        if let Some(reason) = host_format_problem(target_host.unwrap_or_default()) {
            return Err(OagwError::new(
                ErrorKind::InvalidTargetHost,
                "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or \
                     special characters)"
                    .to_owned(),
            )
            .with_context(
                "invalid_value",
                serde_json::json!(target_host.unwrap_or_default()),
            )
            .with_context("reason", serde_json::json!(reason))
            .with_upstream(upstream.id));
        }
        let matching = endpoints
            .iter()
            .find(|endpoint| normalize(&endpoint.host) == requested);
        let Some(endpoint) = matching else {
            return Err(unknown_target_host(upstream, &requested));
        };
        return Ok(endpoint.clone());
    }

    if endpoints.len() == 1 {
        return Ok(endpoints[0].clone());
    }

    if is_common_suffix_alias(&upstream.alias, endpoints) {
        return Err(missing_target_host(upstream));
    }

    let index =
        usize::try_from(round_robin % u64::try_from(endpoints.len()).unwrap_or(1)).unwrap_or(0);
    Ok(endpoints[index].clone())
}

fn valid_hosts(upstream: &Upstream) -> Vec<String> {
    upstream
        .server
        .endpoints
        .iter()
        .map(|endpoint| normalize(&endpoint.host))
        .collect()
}

fn missing_target_host(upstream: &Upstream) -> OagwError {
    OagwError::new(
        ErrorKind::MissingTargetHost,
        "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. \
         Valid hosts: ["
            .to_owned()
            + &valid_hosts(upstream).join(", ")
            + "]",
    )
    .with_context("alias", serde_json::json!(upstream.alias))
    .with_context(
        "valid_hosts",
        serde_json::Value::Array(
            valid_hosts(upstream)
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    )
    .with_upstream(upstream.id)
}

fn unknown_target_host(upstream: &Upstream, requested: &str) -> OagwError {
    OagwError::new(
        ErrorKind::UnknownTargetHost,
        format!(
            "X-OAGW-Target-Host '{}' does not match any configured endpoint. Valid hosts: [{}]",
            requested,
            valid_hosts(upstream).join(", ")
        ),
    )
    .with_context("invalid_value", serde_json::json!(requested))
    .with_context(
        "valid_hosts",
        serde_json::Value::Array(
            valid_hosts(upstream)
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    )
    .with_upstream(upstream.id)
}

/// A short reason when `X-OAGW-Target-Host` is malformed.
#[must_use]
pub fn host_format_problem(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.contains(':') || trimmed.contains('/') || trimmed.contains('?') {
        return Some("header value must not contain a port, path, or query".to_owned());
    }
    validate_host(trimmed)
}

/// Apply `passthrough` rules to the inbound headers, then the configured
/// `set` / `add` / `remove` operations.
#[must_use]
pub fn build_outbound_headers(inbound: &HeaderMap, headers: Option<&HeadersConfig>) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    let rules = headers.map(|configured| &configured.request);
    let passthrough = rules.map_or(Passthrough::None, |request| request.passthrough);
    let allowlist: Option<Vec<String>> = rules.map(|request| {
        request
            .passthrough_allowlist
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect()
    });

    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&lower) {
            continue;
        }
        // The gateway's own headers are consumed, never relayed: `passthrough:
        // all` (or an allowlist naming one) would otherwise hand a caller's
        // routing instruction to the third-party upstream.
        if is_oagw_header(&lower) {
            continue;
        }
        // Headers describing the body being relayed always travel with it.
        let forward = is_body_header(&lower)
            || match passthrough {
                Passthrough::All => true,
                Passthrough::Allowlist => {
                    allowlist.as_ref().is_some_and(|list| list.contains(&lower))
                }
                Passthrough::None => false,
            };
        if forward {
            outbound.append(name.clone(), value.clone());
        }
    }

    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                outbound.remove(parsed);
            }
        }
        for (name, value) in &rules.set {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                outbound.insert(name, value);
            }
        }
        for (name, value) in &rules.add {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                outbound.append(name, value);
            }
        }
    }
    outbound
}

/// Apply the response `set` / `add` / `remove` rules to upstream headers.
pub fn apply_response_headers(headers: &mut HeaderMap, headers_config: Option<&HeadersConfig>) {
    let Some(configured) = headers_config.map(|configured| &configured.response) else {
        return;
    };
    for name in &configured.remove {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(parsed);
        }
    }
    for (name, value) in &configured.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &configured.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Drop hop-by-hop headers from a response about to be returned.
///
/// `Connection` names the fields its sender wants dropped, so the token list is
/// read *before* the removal pass: the pass deletes `connection` itself, and a
/// reader that came after it would always see nothing.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect();
    let hop: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_hop_by_hop(name.as_str()))
        .cloned()
        .collect();
    for name in hop.into_iter().chain(named) {
        headers.remove(name);
    }
}

/// Whether the upstream response should be streamed instead of buffered.
#[must_use]
pub fn is_streaming(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        let lower = value.to_ascii_lowercase();
        lower.contains("text/event-stream")
            || lower.contains("application/stream+json")
            || lower.contains("application/json-seq")
    })
}

/// Attach the `X-OAGW-Error-Source: upstream` header.
pub fn mark_upstream_source(headers: &mut HeaderMap) {
    headers.insert(
        crate::domain::error::ERROR_SOURCE_HEADER,
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );
}

/// Header names allowed into structured logs (never values).
pub const LOGGABLE_HEADERS: [&str; 4] = [
    "content-type",
    "content-length",
    "x-request-id",
    "user-agent",
];

/// Whether a header may appear in a log line.
#[must_use]
pub fn loggable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    LOGGABLE_HEADERS.contains(&lower.as_str())
}

/// Whether an upstream status is an error we must not rewrite.
#[must_use]
pub fn is_upstream_error(status: StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

/// The endpoint host to report in errors.
#[must_use]
pub fn host_of(endpoint: &Endpoint) -> String {
    endpoint.host.clone()
}

/// Canonical error source header value for relayed upstream failures.
pub const UPSTREAM_ERROR_SOURCE: &str = ERROR_SOURCE_UPSTREAM;

/// Extract the scheme prefix for a URL.
trait SchemePrefix {
    fn prefix(&self) -> &'static str;
}

impl SchemePrefix for EndpointScheme {
    /// The scheme actually dialled: `ws` and `grpc` both speak HTTP in
    /// cleartext, so they share `http`'s wire format.
    fn prefix(&self) -> &'static str {
        self.dial_scheme()
    }
}

/// A marker used by tests to assert the hop-by-hop list is complete.
#[must_use]
pub fn hop_by_hop_names() -> Vec<&'static str> {
    HOP_BY_HOP.to_vec()
}

/// Re-export of [`Arc`] for handler signatures.
pub type Shared<T> = Arc<T>;
