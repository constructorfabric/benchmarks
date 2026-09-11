//! Data Plane service contract.
//!
//! The proxy handler turns an inbound request into a [`ProxyRequest`], hands it
//! to the implementation and turns the [`ProxySuccess`] back into a response.
//! WebSocket upgrades are the exception: the handler asks for a
//! [`ResolvedProxy`] and performs the relay itself, because an upgrade is bound
//! to the inbound connection.

use crate::domain::error::OagwError;
use crate::domain::merge::EffectiveConfig;
use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use uuid::Uuid;

/// The normalised inbound proxy request.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Tenant the caller resolved to.
    pub tenant_id: Uuid,
    /// Alias as it appeared in the path (already lowercase).
    pub alias: String,
    /// Path after `/proxy/{alias}`, with no leading slash.
    pub path_suffix: String,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Inbound method.
    pub method: String,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
    /// `X-OAGW-Target-Host`, when the caller pinned an endpoint.
    pub target_host: Option<String>,
    /// Correlation identifier, propagated or generated.
    pub request_id: String,
    /// Whether the request carries a WebSocket `Upgrade`.
    pub is_upgrade: bool,
    /// Whether the request is a CORS preflight.
    pub is_preflight: bool,
}

/// An established streaming body.
pub type BodyStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// A successful proxy response.
#[derive(Debug)]
pub struct ProxySuccess {
    /// Upstream status code.
    pub status: u16,
    /// Response headers after transformation, including the rate-limit and
    /// error-source headers.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: ProxyBody,
}

/// The three body shapes the proxy can return.
pub enum ProxyBody {
    /// A fully buffered body.
    Full(Bytes),
    /// A streamed body (plain HTTP or SSE).
    Streaming(BodyStream),
    /// The upstream leg of a WebSocket relay.
    WebSocket(UpgradedStream),
}

impl std::fmt::Debug for ProxyBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(bytes) => write!(formatter, "Full({} bytes)", bytes.len()),
            Self::Streaming(_) => formatter.write_str("Streaming(..)"),
            Self::WebSocket(_) => formatter.write_str("WebSocket(..)"),
        }
    }
}

/// A duplex byte pipe whose framing belongs to whatever sub-protocol the two
/// ends negotiated. Both the client leg and the upstream leg are presented
/// through it; the combined supertrait exists because a trait object may carry
/// only one non-auto trait.
pub trait DuplexStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send> DuplexStream for T {}

/// An upgraded socket, boxed and pinned for the relay pump.
pub type UpgradedStream = std::pin::Pin<Box<dyn DuplexStream>>;

/// Everything resolved for an upgrade, ready for the outbound handshake.
#[derive(Debug, Clone)]
pub struct ResolvedProxy {
    /// Correlation identifier.
    pub request_id: String,
    /// Endpoint scheme, `http` or `https`.
    pub scheme: String,
    /// Selected endpoint host.
    pub host: String,
    /// Selected endpoint port.
    pub port: u16,
    /// Upstream path including the query string.
    pub path: String,
    /// Transformed request headers to send upstream.
    pub headers: Vec<(String, String)>,
    /// Merged effective configuration.
    pub effective: EffectiveConfig,
}

/// The Data Plane: alias resolution, route matching, policy and forwarding.
#[async_trait]
pub trait DataPlane: Send + Sync {
    /// Handles a non-upgrade request end to end.
    ///
    /// # Errors
    ///
    /// Returns the documented [`OagwError`] for every gateway-generated
    /// failure.
    async fn handle(&self, request: ProxyRequest) -> Result<ProxySuccess, OagwError>;

    /// Resolves the upstream, route and merged configuration for an upgrade.
    ///
    /// # Errors
    ///
    /// Returns the documented [`OagwError`] for every gateway-generated
    /// failure.
    async fn resolve(&self, request: ProxyRequest) -> Result<ResolvedProxy, OagwError>;

    /// Opens the upstream leg of an upgrade and returns its `101` answer.
    ///
    /// The caller relays bytes between the returned socket and its own, because
    /// the inbound leg is bound to the request's connection and never leaves the
    /// transport layer.
    ///
    /// # Errors
    ///
    /// Returns the documented [`OagwError`] for every gateway-generated
    /// failure.
    async fn open_tunnel(&self, request: ProxyRequest) -> Result<UpstreamTunnel, OagwError>;
}

/// An established upstream tunnel: the `101` answer plus its socket.
pub struct UpstreamTunnel {
    /// Upstream status, `101` on success.
    pub status: u16,
    /// Handshake headers to copy back to the caller.
    pub headers: Vec<(String, String)>,
    /// The upgraded upstream socket.
    pub stream: UpgradedStream,
}

impl std::fmt::Debug for UpstreamTunnel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamTunnel")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
