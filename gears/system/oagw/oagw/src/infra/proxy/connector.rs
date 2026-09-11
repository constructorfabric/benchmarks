//! Outbound connection management.
//!
//! Wraps Pingora's connectors so the Data Plane sees one API for the three
//! things it needs: resolve an endpoint to a peer, exchange an HTTP message
//! with a *streaming* response body, and open a raw byte stream for a protocol
//! upgrade.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use http::StatusCode;
use pingora_core::connectors::{ConnectorOptions, TransportConnector, http::Connector};
use pingora_core::listeners::ALPN;
use pingora_core::protocols::Stream as TransportStream;
use pingora_core::protocols::http::client::HttpSession;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::{Endpoint, Scheme};
use crate::domain::plugin::{ProxyRequest, ProxyResponseHead};

/// Response head plus a lazily-consumed body stream.
pub struct UpstreamResponse {
    pub head: ProxyResponseHead,
    pub body: BodyStream,
}

/// Boxed chunk stream over the upstream body.
pub type BodyStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>>;

/// Dials upstream endpoints under the gear's transport policy.
pub struct UpstreamConnector {
    http: Connector,
    transport: TransportConnector,
    connect_timeout: Duration,
    allow_http_upstream: bool,
    ssrf: SsrfPolicy,
}

impl std::fmt::Debug for UpstreamConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamConnector")
            .field("connect_timeout", &self.connect_timeout)
            .field("allow_http_upstream", &self.allow_http_upstream)
            .finish_non_exhaustive()
    }
}

impl UpstreamConnector {
    #[must_use]
    pub fn new(config: &OagwConfig) -> Self {
        Self {
            http: Connector::new(Some(ConnectorOptions::new(128))),
            transport: TransportConnector::new(Some(ConnectorOptions::new(128))),
            connect_timeout: config.connect_timeout(),
            allow_http_upstream: config.allow_http_upstream,
            ssrf: config.ssrf_policy.clone(),
        }
    }

    #[must_use]
    pub fn shared(config: &OagwConfig) -> Arc<Self> {
        Arc::new(Self::new(config))
    }

    /// Resolve `endpoint` and build the peer to dial.
    ///
    /// # Errors
    ///
    /// * `400` — a plaintext scheme while `allow_http_upstream` is off, or an
    ///   address the SSRF policy blocks.
    /// * `503` — the hostname does not resolve.
    pub async fn peer_for(&self, endpoint: &Endpoint) -> OagwResult<HttpPeer> {
        if !endpoint.scheme.is_tls() && !self.allow_http_upstream {
            return Err(OagwError::validation(format!(
                "plaintext upstream '{}' is refused: enable allow_http_upstream to permit \
                 the '{}' scheme",
                endpoint.host,
                endpoint.scheme.as_str()
            )));
        }

        let address = self.resolve(endpoint).await?;
        self.check_ssrf(address, &endpoint.host)?;

        let mut peer = HttpPeer::new(address, endpoint.scheme.is_tls(), endpoint.host.clone());
        peer.options.connection_timeout = Some(self.connect_timeout);
        peer.options.total_connection_timeout = Some(self.connect_timeout);
        // HTTP/1.1 only: it is the version that carries protocol upgrades, and
        // pinning it keeps request framing identical across schemes.
        peer.options.alpn = ALPN::H1;
        Ok(peer)
    }

    async fn resolve(&self, endpoint: &Endpoint) -> OagwResult<SocketAddr> {
        let host = endpoint
            .host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&endpoint.host);

        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, endpoint.port));
        }

        // `HttpPeer::new` panics on an unresolvable name, so resolution
        // happens here where the failure is a normal 503.
        let mut addresses = tokio::net::lookup_host((host, endpoint.port))
            .await
            .map_err(|err| {
                OagwError::new(
                    ErrorKind::LinkUnavailable,
                    format!("could not resolve upstream host '{host}': {err}"),
                )
                .with("host", host.to_owned())
            })?;

        addresses.next().ok_or_else(|| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream host '{host}' resolved to no addresses"),
            )
            .with("host", host.to_owned())
        })
    }

    /// Reject destinations the SSRF policy does not permit.
    fn check_ssrf(&self, address: SocketAddr, host: &str) -> OagwResult<()> {
        if !self.ssrf.enabled {
            return Ok(());
        }
        if self.ssrf.blocked_ports.contains(&address.port()) {
            return Err(OagwError::validation(format!(
                "destination port {} is blocked by the SSRF policy",
                address.port()
            )));
        }
        let refuse = |reason: &str| {
            Err(OagwError::validation(format!(
                "upstream '{host}' resolves to a {reason} address, which the SSRF policy blocks"
            )))
        };
        match address.ip() {
            IpAddr::V4(ip) => {
                if ip.is_loopback() && !self.ssrf.allow_loopback {
                    return refuse("loopback");
                }
                if ip.is_link_local() && !self.ssrf.allow_link_local {
                    return refuse("link-local");
                }
                if ip.is_private() && !self.ssrf.allow_private {
                    return refuse("private");
                }
                if ip.is_unspecified() || ip.is_broadcast() {
                    return refuse("reserved");
                }
            }
            IpAddr::V6(ip) => {
                if ip.is_loopback() && !self.ssrf.allow_loopback {
                    return refuse("loopback");
                }
                // `fe80::/10` — the v6 link-local block, which covers the
                // metadata endpoints.
                let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                if is_link_local && !self.ssrf.allow_link_local {
                    return refuse("link-local");
                }
                // `fc00::/7` — unique local addresses.
                let is_unique_local = (ip.segments()[0] & 0xfe00) == 0xfc00;
                if is_unique_local && !self.ssrf.allow_private {
                    return refuse("private");
                }
                if ip.is_unspecified() {
                    return refuse("reserved");
                }
            }
        }
        Ok(())
    }

    /// Send `request` to `endpoint` and return the response head together with
    /// a streaming body.
    ///
    /// `head_timeout` bounds connect plus response-head read. The body stream
    /// is deliberately unbounded so server-sent-event streams survive.
    ///
    /// # Errors
    ///
    /// `502` on a transport failure, `504` when the head does not arrive in
    /// time.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        request: &ProxyRequest,
        head_timeout: Duration,
    ) -> OagwResult<UpstreamResponse> {
        let peer = self.peer_for(endpoint).await?;
        let exchange = self.exchange(&peer, endpoint, request);

        match tokio::time::timeout(head_timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(OagwError::new(
                ErrorKind::RequestTimeout,
                format!(
                    "upstream '{}' did not send response headers within {}s",
                    endpoint.host,
                    head_timeout.as_secs()
                ),
            )
            .with("host", endpoint.host.clone())
            .with_retry_after(head_timeout.as_secs().max(1))),
        }
    }

    async fn exchange(
        &self,
        peer: &HttpPeer,
        endpoint: &Endpoint,
        request: &ProxyRequest,
    ) -> OagwResult<UpstreamResponse> {
        let (mut session, _reused) = self.http.get_http_session(peer).await.map_err(|err| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("could not connect to upstream '{}': {err}", endpoint.host),
            )
            .with("host", endpoint.host.clone())
        })?;

        let header = build_request_header(endpoint, request)?;
        session
            .write_request_header(Box::new(header))
            .await
            .map_err(|err| transport_error(endpoint, "write request header", &err))?;

        if !request.body.is_empty() {
            session
                .write_request_body(request.body.clone(), true)
                .await
                .map_err(|err| transport_error(endpoint, "write request body", &err))?;
        }
        session
            .finish_request_body()
            .await
            .map_err(|err| transport_error(endpoint, "finish request body", &err))?;

        session
            .read_response_header()
            .await
            .map_err(|err| transport_error(endpoint, "read response header", &err))?;

        let response = session
            .response_header()
            .ok_or_else(|| {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    format!("upstream '{}' returned no response header", endpoint.host),
                )
            })?;

        let status = StatusCode::from_u16(response.status.as_u16()).map_err(|_| {
            OagwError::new(
                ErrorKind::ProtocolError,
                format!("upstream '{}' returned an invalid status", endpoint.host),
            )
        })?;
        let headers = response.headers.clone();

        Ok(UpstreamResponse {
            head: ProxyResponseHead { status, headers },
            body: body_stream(session),
        })
    }

    /// Open a raw byte stream to `endpoint`, for a protocol upgrade.
    ///
    /// # Errors
    ///
    /// `400`/`503` for the same reasons as [`Self::peer_for`], `502` when the
    /// connection cannot be established.
    pub async fn open_stream(&self, endpoint: &Endpoint) -> OagwResult<TransportStream> {
        let peer = self.peer_for(endpoint).await?;
        let connect = self.transport.new_stream(&peer);
        match tokio::time::timeout(self.connect_timeout, connect).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(err)) => Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("could not connect to upstream '{}': {err}", endpoint.host),
            )
            .with("host", endpoint.host.clone())),
            Err(_) => Err(OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!("connecting to upstream '{}' timed out", endpoint.host),
            )
            .with("host", endpoint.host.clone())
            .with_retry_after(self.connect_timeout.as_secs().max(1))),
        }
    }
}

/// Turn the pingora session into a chunk stream that ends on EOF or error.
fn body_stream(session: HttpSession) -> BodyStream {
    Box::pin(futures_util::stream::unfold(
        Some(session),
        |state| async move {
            let mut session = state?;
            match session.read_response_body().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(session))),
                Ok(None) => None,
                // Yield the failure once, then end: re-polling a broken
                // session would spin.
                Err(err) => Some((
                    Err(std::io::Error::other(format!("upstream body error: {err}"))),
                    None,
                )),
            }
        },
    ))
}

fn transport_error(
    endpoint: &Endpoint,
    phase: &str,
    err: &pingora_core::Error,
) -> OagwError {
    OagwError::new(
        ErrorKind::DownstreamError,
        format!(
            "upstream '{}' failed during {phase}: {err}",
            endpoint.host
        ),
    )
    .with("host", endpoint.host.clone())
}

/// Assemble the wire request header, forcing `Host` and the body framing.
fn build_request_header(
    endpoint: &Endpoint,
    request: &ProxyRequest,
) -> OagwResult<RequestHeader> {
    let path_and_query = request.path_and_query();
    let mut header = RequestHeader::build(
        request.method.clone(),
        path_and_query.as_bytes(),
        Some(request.headers.len() + 4),
    )
    .map_err(|err| OagwError::validation(format!("could not build upstream request: {err}")))?;

    for (name, value) in &request.headers {
        header
            .append_header(name.clone(), value.clone())
            .map_err(|err| {
                OagwError::validation(format!("invalid outbound header '{name}': {err}"))
            })?;
    }

    header
        .insert_header(http::header::HOST, upstream_authority(endpoint))
        .map_err(|err| OagwError::validation(format!("invalid Host header: {err}")))?;

    // Explicit framing: pingora picks the request body writer from these
    // headers, so an omitted Content-Length silently drops the body.
    if request.body.is_empty() {
        if matches!(
            request.method,
            http::Method::POST | http::Method::PUT | http::Method::PATCH
        ) {
            let _ = header.insert_header(http::header::CONTENT_LENGTH, "0");
        }
    } else {
        header
            .insert_header(http::header::CONTENT_LENGTH, request.body.len().to_string())
            .map_err(|err| OagwError::validation(format!("invalid Content-Length: {err}")))?;
    }

    Ok(header)
}

/// `Host` value for the upstream: hostname, plus port when non-standard.
#[must_use]
pub fn upstream_authority(endpoint: &Endpoint) -> String {
    let standard = match endpoint.scheme {
        Scheme::Http | Scheme::Ws => 80,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => 443,
    };
    if endpoint.port == standard {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

#[cfg(test)]
#[path = "connector_tests.rs"]
mod tests;
