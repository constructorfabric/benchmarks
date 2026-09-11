//! Upstream connection handling.
//!
//! The gear dials upstreams through pingora's HTTP connector, which pools
//! keep-alive sessions and negotiates TLS with rustls. Plaintext dials are
//! refused unless the gear was configured with `allow_http_upstream`.

use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::time::Duration;

use bytes::Bytes;
use pingora_core::connectors::http::Connector as HttpConnector;
use pingora_core::connectors::ConnectorOptions;
use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::http::client::HttpSession;
use pingora_core::protocols::tls::ALPN;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;

use crate::config::OagwConfig;
use crate::domain::error::DomainError;

/// An HTTP/1.1 request ready to be written to an upstream.
#[derive(Debug, Clone, Default)]
pub struct UpstreamRequest {
    /// Request method, e.g. `GET`.
    pub method: String,
    /// Request target without the query string.
    pub path: String,
    /// Raw query string, without the leading `?`.
    pub query: Option<String>,
    /// Header pairs in wire order.
    pub headers: Vec<(String, String)>,
    /// Request body, when the method or headers admit one.
    pub body: Option<Bytes>,
}

impl UpstreamRequest {
    /// The request target as it goes on the wire.
    pub fn target(&self) -> String {
        match self.query.as_deref().filter(|q| !q.is_empty()) {
            Some(query) => format!("{}?{}", self.path, query),
            None => self.path.clone(),
        }
    }
}

/// A dialled upstream exchange: response headers have been read and the body
/// is still to be drained.
pub struct UpstreamExchange {
    session: HttpSession<()>,
}

impl UpstreamExchange {
    /// The response status code.
    pub fn status(&self) -> u16 {
        self.session
            .response_header()
            .map(|head| head.status.as_u16())
            .unwrap_or(500)
    }

    /// Snapshot of the response headers, in wire order, lower-cased.
    pub fn headers(&self) -> Vec<(String, String)> {
        self.session
            .response_header()
            .map(|head| {
                head.headers
                    .iter()
                    .filter_map(|(name, value)| {
                        value
                            .to_str()
                            .ok()
                            .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The first value of a response header.
    pub fn header(&self, name: &str) -> Option<String> {
        self.session
            .response_header()
            .and_then(|head| {
                head.headers
                    .get(name)
                    .and_then(|value| value.to_str().ok().map(|v| v.to_string()))
            })
    }

    /// Every value of a response header.
    pub fn header_all(&self, name: &str) -> Vec<String> {
        self.session
            .response_header()
            .map(|head| {
                head.headers
                    .get_all(name)
                    .iter()
                    .filter_map(|value| value.to_str().ok().map(|v| v.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Buffers the whole response body, capped at `limit` bytes.
    pub async fn body_bytes(&mut self, limit: usize) -> Result<Bytes, DomainError> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.session.read_response_body().await {
                Ok(Some(chunk)) => {
                    if out.len() + chunk.len() > limit {
                        return Err(DomainError::PayloadTooLarge {
                            detail: format!(
                                "the upstream response body exceeds {limit} bytes"
                            ),
                        });
                    }
                    out.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(DomainError::DownstreamError {
                        status: 502,
                        host: Some(String::new()),
                    });
                }
            }
        }
        Ok(Bytes::from(out))
    }

    /// Streams the response body, decoding chunked transfer encoding.
    pub fn into_body_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<Bytes, anyhow::Error>> + Send + 'static {
        futures_util::stream::unfold(self, |mut exchange| async move {
            match exchange.session.read_response_body().await {
                Ok(Some(chunk)) => Some((Ok(chunk), exchange)),
                Ok(None) => None,
                Err(err) => Some((
                    Err(anyhow::anyhow!("upstream body read failed: {err}")),
                    exchange,
                )),
            }
        })
    }

}

/// The dialling side of the data plane.
pub struct UpstreamConnector {
    http: HttpConnector,
    transport: TransportConnector,
    allow_http: bool,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
}

impl std::fmt::Debug for UpstreamConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamConnector")
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl UpstreamConnector {
    /// Builds a connector from the gear configuration.
    pub fn new(config: &OagwConfig) -> Self {
        let timeout = Some(Duration::from_secs(config.proxy_timeout_secs.max(1)));
        let pool = Some(ConnectorOptions::new(config.pool_size.max(1)));
        Self {
            http: HttpConnector::new(pool.clone()),
            transport: TransportConnector::new(pool),
            allow_http: config.allow_http_upstream,
            connect_timeout: timeout,
            read_timeout: timeout,
            write_timeout: timeout,
            idle_timeout: timeout,
        }
    }

    /// Whether a plaintext dial is permitted.
    pub fn allows_plaintext(&self) -> bool {
        self.allow_http
    }

    /// Resolves and builds the peer for a host/port pair.
    pub fn peer(&self, host: &str, port: u16, tls: bool) -> Result<HttpPeer, DomainError> {
        if !tls && !self.allow_http {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "plaintext upstream connections are disabled; set allow_http_upstream to dial {host}:{port}"
                ),
            });
        }
        let address: SocketAddr = resolve(host, port)?;
        let mut peer = HttpPeer::new((address.ip(), address.port()), tls, host.to_string());
        peer.options.alpn = ALPN::H1;
        peer.options.connection_timeout = self.connect_timeout;
        peer.options.total_connection_timeout = self.connect_timeout;
        peer.options.read_timeout = self.read_timeout;
        peer.options.write_timeout = self.write_timeout;
        peer.options.idle_timeout = self.idle_timeout;
        Ok(peer)
    }

    /// Opens a raw transport stream, used for upgraded connections.
    pub async fn open_raw(&self, peer: &HttpPeer) -> Result<Stream, DomainError> {
        self.transport.new_stream(peer).await.map_err(|err| {
            DomainError::LinkUnavailable {
                detail: format!("could not connect to upstream: {err}"),
                upstream_id: Some(String::new()),
                alias: Some(String::new()),
            }
        })
    }

    /// Dials the upstream and writes the request, returning the exchange once
    /// the response headers have been read.
    pub async fn exchange(
        &self,
        peer: &HttpPeer,
        request: UpstreamRequest,
    ) -> Result<UpstreamExchange, DomainError> {
        let mut header =
            RequestHeader::build(request.method.as_str(), request.target().as_bytes(), None)
                .map_err(|err| DomainError::ProtocolError {
                    detail: format!("could not build the upstream request: {err}"),
                    host: Some(peer.sni.clone()),
                })?;
        let headers = request.headers.clone();
        for (name, value) in &headers {
            header.insert_header(name.clone(), value.as_str()).ok();
        }
        // The outbound header list drops the caller's `Content-Length` because
        // the plugin chain may have rewritten the body, so the length of what
        // is actually sent is declared here. Without it the request reads as
        // bodyless and the payload never leaves.
        let body = request.body.filter(|b| !b.is_empty());
        if let Some(body) = &body {
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-length")) {
                header
                    .insert_header("content-length", body.len().to_string())
                    .ok();
            }
        }
        let mut session = self
            .http
            .get_http_session(peer)
            .await
            .map_err(|err| DomainError::LinkUnavailable {
                detail: format!("could not connect to upstream: {err}"),
                upstream_id: Some(String::new()),
                alias: Some(String::new()),
            })?
            .0;
        let host = Some(peer.sni.clone());
        session
            .write_request_header(Box::new(header))
            .await
            .map_err(write_error(host.clone()))?;
        if let Some(body) = body {
            session
                .write_request_body(body, true)
                .await
                .map_err(write_error(host.clone()))?;
        }
        session
            .finish_request_body()
            .await
            .map_err(write_error(host.clone()))?;
        session
            .read_response_header()
            .await
            .map_err(|_| DomainError::DownstreamError { status: 502, host })?;
        Ok(UpstreamExchange { session })
    }
}

fn write_error(
    host: Option<String>,
) -> impl FnOnce(Box<pingora_core::Error>) -> DomainError {
    move |err| DomainError::ProtocolError {
        detail: format!("could not send the upstream request: {err}"),
        host,
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, DomainError> {
    format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|err| DomainError::LinkUnavailable {
            detail: format!("{host}:{port} could not be resolved: {err}"),
            upstream_id: Some(String::new()),
            alias: Some(String::new()),
        })?
        .next()
        .ok_or_else(|| DomainError::LinkUnavailable {
            detail: format!("{host}:{port} resolved to no address"),
            upstream_id: Some(String::new()),
            alias: Some(String::new()),
        })
}
