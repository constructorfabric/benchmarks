//! Outbound transport: endpoint selection, dialling and tunnelling.
//!
//! Selection honours an explicit `X-OAGW-Target-Host` before the configured
//! load-balancing strategy, and every dial carries the gear's plaintext policy
//! and the effective timeout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use http::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, LoadBalancing, Scheme, Upstream};
use crate::domain::services::data_plane::{DuplexStream, EndpointSelector};

/// Request header naming the endpoint an exchange is forwarded to.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Whether a scheme is dialled in the clear.
#[must_use]
pub const fn is_plaintext(scheme: Scheme) -> bool {
    matches!(scheme, Scheme::Http)
}

/// Selects endpoints from a pool.
#[derive(Debug, Default)]
pub struct PoolSelector {
    cursor: AtomicU64,
}

impl PoolSelector {
    /// A fresh selector.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cursor: AtomicU64::new(0),
        }
    }

    /// Every endpoint matching `host`, by authority first then by host name.
    #[must_use]
    pub fn candidates<'a>(upstream: &'a Upstream, host: &str) -> Vec<&'a Endpoint> {
        let wanted = host.trim().to_ascii_lowercase();
        let exact: Vec<&Endpoint> = upstream
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.host_authority() == wanted)
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        upstream
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.host.eq_ignore_ascii_case(&wanted))
            .collect()
    }

    /// Whether `value` is a bare host or IP literal.
    #[must_use]
    pub fn is_bare_host(value: &str) -> bool {
        let trimmed = value.trim();
        !trimmed.is_empty()
            && trimmed
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    }

    /// Apply the load-balancing strategy across a non-empty candidate set.
    #[must_use]
    pub fn balance<'a>(
        &self,
        strategy: LoadBalancing,
        candidates: &[&'a Endpoint],
    ) -> &'a Endpoint {
        if candidates.len() < 2 {
            return candidates[0];
        }
        match strategy {
            LoadBalancing::Random => {
                let pick = self.next_random() % u64::try_from(candidates.len()).unwrap_or(u64::MAX);
                candidates[usize::try_from(pick).unwrap_or(0)]
            }
            LoadBalancing::LeastConnections => candidates
                .iter()
                .copied()
                .min_by_key(|endpoint| (endpoint.priority, endpoint.weight))
                .unwrap_or(candidates[0]),
            LoadBalancing::RoundRobin => {
                let turn = self.cursor.fetch_add(1, Ordering::Relaxed)
                    % u64::try_from(candidates.len()).unwrap_or(u64::MAX);
                candidates[usize::try_from(turn).unwrap_or(0)]
            }
        }
    }

    fn next_random(&self) -> u64 {
        // xorshift64*: cheap, dependency-free and enough to spread load across
        // a pool; this is not a security primitive.
        let mut state = self
            .cursor
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            | 1;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

impl EndpointSelector for PoolSelector {
    fn select(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        if upstream.endpoints.is_empty() {
            return Err(DomainError::validation("upstream has no endpoints"));
        }
        let Some(explicit) = target_host.map(str::trim).filter(|value| !value.is_empty()) else {
            let mut hosts: Vec<String> = upstream
                .endpoints
                .iter()
                .map(|endpoint| endpoint.host.to_ascii_lowercase())
                .collect();
            hosts.sort();
            hosts.dedup();
            if hosts.len() > 1 {
                return Err(DomainError::missing_target_host(format!(
                    "upstream '{}' spans {} hosts; set the '{TARGET_HOST_HEADER}' header",
                    upstream.alias,
                    hosts.len()
                )));
            }
            // One host, several endpoints: the load-balancing strategy walks it.
            let mut candidates: Vec<&Endpoint> = upstream.endpoints.iter().collect();
            candidates.sort_by_key(|endpoint| (endpoint.priority, endpoint.weight));
            return Ok(self.balance(upstream.load_balancing, &candidates).clone());
        };

        if !Self::is_bare_host(explicit) {
            return Err(DomainError::invalid_target_host(format!(
                "'{explicit}' is not a bare host or IP literal"
            )));
        }
        let candidates = Self::candidates(upstream, explicit);
        if candidates.is_empty() {
            return Err(DomainError::unknown_target_host(format!(
                "'{explicit}' is not an endpoint of upstream '{}'",
                upstream.alias
            )));
        }
        Ok(self.balance(upstream.load_balancing, &candidates).clone())
    }
}

/// The outcome of dialling an upgrade: what the upstream answered and, when it
/// agreed, the raw connection to bridge.
pub struct UpstreamUpgrade {
    /// Status the upstream answered with.
    pub status: StatusCode,
    /// Upstream response headers.
    pub headers: http::HeaderMap,
    /// The raw upstream connection, present only on `101 Switching Protocols`.
    pub stream: Option<DuplexStream>,
}

/// Connects to an upstream endpoint and carries an exchange over it.
#[derive(Debug, Clone, Copy)]
pub struct OutboundTransport {
    allow_http: bool,
    default_timeout_secs: u64,
}

impl OutboundTransport {
    /// A transport governed by `allow_http` and `default_timeout_secs`.
    #[must_use]
    pub const fn new(allow_http: bool, default_timeout_secs: u64) -> Self {
        Self {
            allow_http,
            default_timeout_secs,
        }
    }

    /// Timeout for a route timeout, falling back to the gear's.
    #[must_use]
    pub fn timeout_for(&self, route_timeout: Option<u64>) -> Duration {
        route_timeout.map_or(
            Duration::from_secs(self.default_timeout_secs),
            Duration::from_secs,
        )
    }

    /// Whether a connection to this endpoint is allowed by policy.
    #[must_use]
    pub const fn allows(&self, endpoint: &Endpoint) -> bool {
        !is_plaintext(endpoint.scheme) || self.allow_http
    }

    /// Forward a request to `endpoint`, streaming the response body back.
    ///
    /// # Errors
    /// Returns the documented transport error kinds.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        request: http::Request<axum::body::Body>,
        timeout: Duration,
    ) -> Result<http::Response<axum::body::Body>, DomainError> {
        if !self.allows(endpoint) {
            return Err(plaintext_refused(endpoint));
        }
        let io = dial(endpoint, timeout).await?;
        exchange(io, request, timeout).await
    }

    /// Dial the upstream and send an upgrade request, returning its answer.
    ///
    /// # Errors
    /// Returns [`ErrorKind::LinkUnavailable`] when the upstream cannot be
    /// reached and [`ErrorKind::Protocol`] when its answer is unparseable.
    pub async fn dial_upgrade(
        &self,
        endpoint: &Endpoint,
        request: &http::request::Parts,
        timeout: Duration,
    ) -> Result<UpstreamUpgrade, DomainError> {
        if !self.allows(endpoint) {
            return Err(plaintext_refused(endpoint));
        }
        let mut io = dial(endpoint, timeout).await?;
        let head = render_request_head(request)?;
        io.write_all(&head).await.map_err(|err| {
            DomainError::link_unavailable(format!("upstream write failed: {err}"))
        })?;

        let raw = read_response_head(&mut io, timeout).await?;
        let (status, headers) = parse_response_head(&raw)?;
        let stream = (status == StatusCode::SWITCHING_PROTOCOLS).then_some(Box::new(io) as _);
        Ok(UpstreamUpgrade {
            status,
            headers,
            stream,
        })
    }

    /// Bridge two upgraded connections until either side closes.
    ///
    /// # Errors
    /// Returns [`ErrorKind::StreamAborted`] when either leg fails.
    pub async fn bridge(
        &self,
        client: &mut DuplexStream,
        upstream: &mut DuplexStream,
    ) -> Result<(), DomainError> {
        match tokio::io::copy_bidirectional(client, upstream).await {
            Ok(_) => Ok(()),
            Err(err) => Err(DomainError::stream_aborted(format!("tunnel closed: {err}"))),
        }
    }
}

fn plaintext_refused(endpoint: &Endpoint) -> DomainError {
    DomainError::link_unavailable(format!(
        "plaintext connections to '{}' are not allowed by this gateway",
        endpoint.host
    ))
}

async fn dial(endpoint: &Endpoint, timeout: Duration) -> Result<UpstreamIo, DomainError> {
    let address = (endpoint.host.as_str(), endpoint.port_or_default());
    let connect = tokio::net::TcpStream::connect(address);
    let stream = match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            return Err(DomainError::link_unavailable(format!(
                "cannot reach '{}': {err}",
                endpoint.host_authority()
            )));
        }
        Err(_) => {
            return Err(DomainError::connection_timeout(format!(
                "'{}' did not answer within {timeout:?}",
                endpoint.host_authority()
            )));
        }
    };
    stream.set_nodelay(true).ok();
    if is_plaintext(endpoint.scheme) {
        return Ok(UpstreamIo::Plain(stream));
    }
    tls_dial(endpoint, stream).await
}

async fn tls_dial(
    endpoint: &Endpoint,
    stream: tokio::net::TcpStream,
) -> Result<UpstreamIo, DomainError> {
    let connector = tokio_rustls::TlsConnector::from(tls_config()?);
    let name = tls_server_name(&endpoint.host)?;
    let tls = connector.connect(name, stream).await.map_err(|err| {
        DomainError::link_unavailable(format!(
            "TLS handshake with '{}' failed: {err}",
            endpoint.host
        ))
    })?;
    Ok(UpstreamIo::Tls(Box::new(tls)))
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, DomainError> {
    static CONFIG: LazyLock<Option<Arc<rustls::ClientConfig>>> = LazyLock::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder_with_provider(Arc::clone(provider()))
            .with_safe_default_protocol_versions()
            .ok()
            .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
            .map(Arc::new)
    });
    CONFIG
        .clone()
        .ok_or_else(|| DomainError::link_unavailable("no TLS trust store is available"))
}

/// The crypto provider, installed once and reused by every dial.
///
/// [`toolkit::bootstrap::init_crypto_provider`] installs the platform's
/// provider at startup; when it has not run, the rustls default is minted here
/// so a gateway that boots without the bootstrap still dials TLS.
fn provider() -> &'static Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: LazyLock<Arc<rustls::crypto::CryptoProvider>> = LazyLock::new(|| {
        rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
    });
    &PROVIDER
}

fn tls_server_name(host: &str) -> Result<rustls_pki_types::ServerName<'static>, DomainError> {
    let name = host
        .trim()
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']');
    rustls_pki_types::ServerName::try_from(name.to_owned())
        .map_err(|_| DomainError::invalid_target_host(format!("'{host}' is not a TLS name")))
}

async fn exchange(
    io: UpstreamIo,
    request: http::Request<axum::body::Body>,
    timeout: Duration,
) -> Result<http::Response<axum::body::Body>, DomainError> {
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|err| DomainError::link_unavailable(format!("upstream refused: {err}")))?;
    tokio::spawn(async move {
        // A per-request connection holds no pooled state, so an error here is
        // simply the end of its life.
        connection.with_upgrades().await.ok();
    });
    let response = tokio::time::timeout(timeout, sender.send_request(request))
        .await
        .map_err(|_| DomainError::request_timeout("upstream did not answer in time"))?
        .map_err(|err| DomainError::protocol(format!("upstream spoke no HTTP: {err}")))?;
    Ok(response.map(axum::body::Body::new))
}

async fn read_response_head(
    io: &mut UpstreamIo,
    timeout: Duration,
) -> Result<Vec<u8>, DomainError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(timeout, io.read(&mut chunk))
            .await
            .map_err(|_| DomainError::request_timeout("upstream did not answer the upgrade"))?
            .map_err(|err| DomainError::link_unavailable(format!("upstream read failed: {err}")))?;
        if read == 0 {
            return Err(DomainError::link_unavailable(
                "upstream closed before answering the upgrade",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if head_complete(&buffer) {
            return Ok(buffer);
        }
    }
}

fn head_complete(raw: &[u8]) -> bool {
    raw.windows(4).any(|window| window == b"\r\n\r\n")
        || raw.windows(2).any(|window| window == b"\n\n")
}

fn parse_response_head(raw: &[u8]) -> Result<(StatusCode, http::HeaderMap), DomainError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map_or_else(
            || {
                raw.windows(2)
                    .position(|window| window == b"\n\n")
                    .map_or(raw.len(), |index| index + 2)
            },
            |index| index + 4,
        );
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| DomainError::protocol("upstream answer was not ASCII"))?;
    let mut lines = head
        .split("\r\n")
        .flat_map(|line| line.split('\n'))
        .filter(|line| !line.is_empty());

    let status_line = lines
        .next()
        .ok_or_else(|| DomainError::protocol("upstream answered with no status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .ok_or_else(|| DomainError::protocol("upstream status line was malformed"))?;

    let mut headers = http::HeaderMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = http::HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| DomainError::protocol("upstream sent an invalid header name"))?;
        let value = http::HeaderValue::from_str(value.trim())
            .map_err(|_| DomainError::protocol("upstream sent an invalid header value"))?;
        headers.append(name, value);
    }
    Ok((status, headers))
}

/// Serialise a request head for the wire, restoring the `Host` header.
fn render_request_head(request: &http::request::Parts) -> Result<Vec<u8>, DomainError> {
    let path = request
        .uri
        .path_and_query()
        .map_or_else(|| request.uri.path(), |pq| pq.as_str());
    let mut head = format!("{} {path} HTTP/1.1\r\n", request.method.as_str());

    let authority = request
        .uri
        .authority()
        .map_or(String::new(), |value| value.as_str().to_owned());
    if !authority.is_empty() && request.headers.get(http::header::HOST).is_none() {
        head.push_str("Host: ");
        head.push_str(&authority);
        head.push_str("\r\n");
    }
    for (name, value) in &request.headers {
        if name == http::header::HOST {
            continue;
        }
        let rendered = value
            .to_str()
            .map_err(|_| DomainError::validation("request header is not valid ASCII"))?;
        head.push_str(name.as_str());
        head.push_str(": ");
        head.push_str(rendered);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    Ok(head.into_bytes())
}

/// The upstream connection: plaintext TCP or a TLS session over it.
pub enum UpstreamIo {
    /// A plaintext TCP stream.
    Plain(tokio::net::TcpStream),
    /// A TLS session established over a TCP stream.
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl tokio::io::AsyncRead for UpstreamIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Both sides of the enum are `Unpin`, so the projection is a plain
        // `&mut` and the inner stream can be re-pinned directly.
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(&mut **stream).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for UpstreamIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(&mut **stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => std::pin::Pin::new(&mut **stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => std::pin::Pin::new(&mut **stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod outbound_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;

    fn pool(endpoints: &[&str]) -> Upstream {
        let mut result = Upstream::for_test();
        result.endpoints = endpoints
            .iter()
            .map(|host| Endpoint {
                host: (*host).to_owned(),
                ..Endpoint::default()
            })
            .collect();
        result
    }

    #[test]
    fn a_bare_host_is_one_with_no_separators() {
        assert!(PoolSelector::is_bare_host("alpha.internal"));
        assert!(PoolSelector::is_bare_host("10.0.0.7"));
        assert!(!PoolSelector::is_bare_host("alpha.internal:8080"));
        assert!(!PoolSelector::is_bare_host("alpha.internal/api"));
        assert!(!PoolSelector::is_bare_host(""));
        assert!(!PoolSelector::is_bare_host("al pha"));
    }

    #[test]
    fn an_explicit_target_host_wins_over_the_pool() {
        let selector = PoolSelector::new();
        let upstream = pool(&["a.internal", "b.internal"]);
        let endpoint = selector.select(&upstream, Some("b.internal")).unwrap();
        assert_eq!(endpoint.host, "b.internal");
    }

    #[test]
    fn a_host_with_a_port_is_rejected() {
        let selector = PoolSelector::new();
        let upstream = pool(&["a.internal", "b.internal"]);
        let err = selector
            .select(&upstream, Some("b.internal:8080"))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidTargetHost);
    }

    #[test]
    fn an_unknown_target_host_is_rejected() {
        let selector = PoolSelector::new();
        let upstream = pool(&["a.internal", "b.internal"]);
        let err = selector.select(&upstream, Some("c.internal")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownTargetHost);
    }

    #[test]
    fn a_multi_host_pool_demands_an_explicit_target() {
        let selector = PoolSelector::new();
        let upstream = pool(&["a.internal", "b.internal"]);
        let err = selector.select(&upstream, None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingTargetHost);
        assert!(
            err.detail().contains(TARGET_HOST_HEADER),
            "the rejection names the header that fixes it"
        );
    }

    #[test]
    fn a_single_host_pool_needs_no_explicit_target() {
        let selector = PoolSelector::new();
        let upstream = pool(&["a.internal", "a.internal"]);
        let endpoint = selector.select(&upstream, None).unwrap();
        assert_eq!(endpoint.host, "a.internal");
    }

    #[test]
    fn the_preferred_endpoint_wins_without_a_target_header() {
        let selector = PoolSelector::new();
        let mut upstream = pool(&["a.internal", "a.internal"]);
        upstream.endpoints[0].port = Some(80);
        upstream.endpoints[0].priority = 5;
        upstream.endpoints[1].port = Some(8080);
        upstream.endpoints[1].priority = 1;
        let endpoint = selector.select(&upstream, None).unwrap();
        assert_eq!(endpoint.port_or_default(), 8080);
    }

    #[test]
    fn an_empty_pool_is_a_validation_error() {
        let selector = PoolSelector::new();
        let mut upstream = Upstream::for_test();
        upstream.endpoints.clear();
        let err = selector.select(&upstream, None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
    }

    #[test]
    fn round_robin_walks_the_candidates() {
        let selector = PoolSelector::new();
        let mut upstream = pool(&["a.internal", "a.internal"]);
        upstream.endpoints[0].port = Some(80);
        upstream.endpoints[1].port = Some(8080);
        let first = selector.select(&upstream, None).unwrap();
        let second = selector.select(&upstream, None).unwrap();
        assert_ne!(first.port_or_default(), second.port_or_default());
    }

    #[test]
    fn the_transport_refuses_plaintext_when_it_is_disabled() {
        let transport = OutboundTransport::new(false, 5);
        let plain = Endpoint {
            scheme: Scheme::Http,
            ..Endpoint::default()
        };
        let secure = Endpoint {
            scheme: Scheme::Https,
            ..Endpoint::default()
        };
        assert!(!transport.allows(&plain));
        assert!(transport.allows(&secure));

        let permissive = OutboundTransport::new(true, 5);
        assert!(permissive.allows(&plain));
    }

    #[test]
    fn the_route_timeout_wins_over_the_default() {
        let transport = OutboundTransport::new(true, 30);
        assert_eq!(transport.timeout_for(Some(7)), Duration::from_secs(7));
        assert_eq!(transport.timeout_for(None), Duration::from_secs(30));
    }

    #[test]
    fn an_ipv6_literal_is_a_valid_tls_name() {
        assert!(tls_server_name("api.partner.com").is_ok());
        assert!(tls_server_name("[::1]").is_ok());
        assert!(tls_server_name("bad host").is_err());
    }
}
