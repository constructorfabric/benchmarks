//! The data plane contract: what a proxied exchange needs before any bytes
//! move.
//!
//! [`ProxyPlan`] is the pure-domain decision — route, upstream, endpoint and
//! forwarded path. The traits below are the seams the infrastructure
//! implementation in [`crate::infra::proxy`] fills in: rate limiting,
//! credential resolution and the outbound transport.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, RateLimit, Route, Upstream};

/// Where a token bucket lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RateLimitKey {
    /// One bucket per tenant.
    Tenant(uuid::Uuid),
    /// One bucket per authenticated subject.
    Subject(uuid::Uuid, uuid::Uuid),
    /// One bucket per client address.
    Ip(String),
    /// One bucket per route, within a tenant.
    Route(uuid::Uuid, uuid::Uuid),
    /// One bucket per upstream, within a tenant.
    Upstream(uuid::Uuid, uuid::Uuid),
}

impl std::fmt::Display for RateLimitKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tenant(t) => write!(f, "tenant:{t}"),
            Self::Subject(t, s) => write!(f, "subject:{t}:{s}"),
            Self::Ip(ip) => write!(f, "ip:{ip}"),
            Self::Route(t, r) => write!(f, "route:{t}:{r}"),
            Self::Upstream(t, u) => write!(f, "upstream:{t}:{u}"),
        }
    }
}

impl RateLimitKey {
    /// The key a [`RateLimit`] configuration resolves to.
    #[must_use]
    pub fn resolve(limit: &RateLimit, tenant: uuid::Uuid, subject: uuid::Uuid, ip: &str) -> Self {
        match limit.scope {
            crate::domain::model::RateLimitScope::Tenant => Self::Tenant(tenant),
            crate::domain::model::RateLimitScope::Subject => Self::Subject(tenant, subject),
            crate::domain::model::RateLimitScope::Ip => Self::Ip(ip.to_owned()),
        }
    }

    /// The resource discriminator appended to the key.
    #[must_use]
    pub fn with_resource(&self, resource: &str) -> String {
        format!("{self}:{resource}")
    }
}

/// Outcome of asking a bucket for `cost` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitOutcome {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Tokens left in the bucket after the request.
    pub remaining: u32,
    /// Epoch seconds at which the bucket is full again.
    pub reset_at: u64,
    /// How long the caller should wait before retrying, in seconds.
    pub retry_after_secs: u64,
    /// Capacity the limit advertises in `X-RateLimit-Limit`.
    pub capacity: u32,
}

/// Token buckets, keyed by [`RateLimitKey`].
#[async_trait]
pub trait RateLimiter: Send + Sync {
    /// Take `cost` tokens from the bucket for `key`, refilling it first.
    async fn acquire(&self, key: &str, limit: &RateLimit, cost: u32) -> RateLimitOutcome;
}

/// Resolves a `cred://` reference to its value at request time.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// The secret's value. Implementations must never log it.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::SecretNotFound`] when the
    /// reference resolves to nothing.
    async fn resolve(&self, ctx: &SecurityContext, secret_ref: &str)
    -> Result<String, DomainError>;
}

/// The decision the data plane reached before forwarding.
#[derive(Debug, Clone)]
pub struct ProxyPlan {
    /// The route that won.
    pub route: Route,
    /// The upstream the route targets.
    pub upstream: Upstream,
    /// The endpoint selected from the pool.
    pub endpoint: Endpoint,
    /// Path forwarded to the upstream, including any target prefix.
    pub forward_path: String,
    /// Effective rate limit after the hierarchical merge.
    pub rate_limit: Option<RateLimit>,
}

impl ProxyPlan {
    /// Whether the exchange is a WebSocket upgrade.
    #[must_use]
    pub fn is_websocket(headers: &http::HeaderMap) -> bool {
        headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }

    /// The bucket key for this plan, given the caller.
    #[must_use]
    pub fn limit_key(&self, ctx: &SecurityContext, client_ip: &str) -> Option<String> {
        let limit = self.rate_limit.as_ref()?;
        let base =
            RateLimitKey::resolve(limit, self.upstream.tenant_id, ctx.subject_id(), client_ip);
        let resource = match limit.sharing {
            crate::domain::model::RateLimitSharing::PerRoute => {
                Some(format!("route:{}", self.route.id))
            }
            crate::domain::model::RateLimitSharing::PerUpstream => {
                Some(format!("upstream:{}", self.upstream.id))
            }
            crate::domain::model::RateLimitSharing::Shared => None,
        };
        Some(match resource {
            Some(resource) => base.with_resource(&resource),
            None => base.to_string(),
        })
    }
}

/// Selects the endpoint an exchange is forwarded to.
pub trait EndpointSelector: Send + Sync {
    /// One endpoint of `upstream`, honouring an explicit
    /// `X-OAGW-Target-Host` when present and the load-balancing strategy
    /// otherwise.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::MissingTargetHost`] when the
    /// pool needs an explicit choice and none was given,
    /// [`crate::domain::error::ErrorKind::InvalidTargetHost`] when the header is
    /// malformed and [`crate::domain::error::ErrorKind::UnknownTargetHost`] when
    /// it names no configured endpoint.
    fn select(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, DomainError>;
}

/// A bidirectional byte stream, as an upgrade bridge sees it.
///
/// `AsyncRead` and `AsyncWrite` cannot both name a trait object, so the two
/// are folded into one local supertrait every concrete stream implements.
pub trait AsyncReadWrite: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T> AsyncReadWrite for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

/// The client side of an upgraded connection, as raw bytes.
pub type DuplexStream = Box<dyn AsyncReadWrite>;

/// What an upstream answered to an upgrade request.
///
/// `stream` is present only when the upstream agreed with `101 Switching
/// Protocols`; the caller then owns both legs of the tunnel.
pub struct UpstreamHandshake {
    /// Status the upstream answered with.
    pub status: http::StatusCode,
    /// Upstream response headers, echoed to the caller on `101`.
    pub headers: http::HeaderMap,
    /// The raw upstream connection, present only on an agreed upgrade.
    pub stream: Option<DuplexStream>,
}

impl std::fmt::Debug for UpstreamHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The tunnel itself is never rendered: it is a live socket, not data.
        f.debug_struct("UpstreamHandshake")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("stream", &self.stream.as_ref().map(|_| "tunnel"))
            .finish()
    }
}

/// A gateway failure plus the headers the plugin chain attached to it.
///
/// `transform_error` may add headers (a correlation identifier, a hint) that
/// the rendered problem document must carry, so they travel with the error.
#[derive(Debug, Clone)]
pub struct GatewayFailure {
    /// The error to render.
    pub error: DomainError,
    /// Extra headers the plugin chain asked for.
    pub headers: http::HeaderMap,
}

impl GatewayFailure {
    /// Wrap `error` with no extra headers.
    #[must_use]
    pub fn new(error: DomainError) -> Self {
        Self {
            error,
            headers: http::HeaderMap::new(),
        }
    }
}

impl From<DomainError> for GatewayFailure {
    fn from(error: DomainError) -> Self {
        Self::new(error)
    }
}

/// A shared, cloneable handle to the data plane.
pub type SharedDataPlane = Arc<dyn DataPlaneService>;

/// Executes a proxied exchange.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Plan the exchange without moving any bytes.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::RouteNotFound`] when no route
    /// matches and [`ErrorKind::UnknownTargetHost`] for an unknown target host.
    async fn plan(
        &self,
        ctx: &SecurityContext,
        method: &str,
        alias: &str,
        path: &str,
        target_host: Option<&str>,
    ) -> Result<ProxyPlan, DomainError>;

    /// Forward a request to the planned endpoint and return the upstream's
    /// response, body included as a stream.
    ///
    /// # Errors
    /// Returns a [`GatewayFailure`] carrying the documented error kinds for
    /// transport and protocol failures.
    async fn forward(
        &self,
        ctx: &SecurityContext,
        plan: &ProxyPlan,
        request: http::Request<axum::body::Body>,
    ) -> Result<http::Response<axum::body::Body>, GatewayFailure>;

    /// Dial the upstream for an upgrade and send its request head.
    ///
    /// The caller renders the answer to the client itself, so a `101` is only
    /// sent once the upstream has agreed.
    ///
    /// # Errors
    /// Returns the [`DomainError`] kinds documented in `contracts/errors.md`
    /// for transport and protocol failures.
    async fn open_tunnel(
        &self,
        ctx: &SecurityContext,
        plan: &ProxyPlan,
        request: &http::request::Parts,
    ) -> Result<UpstreamHandshake, DomainError>;

    /// Bridge the two legs of an agreed upgrade until either side closes.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::StreamAborted`] when a leg
    /// fails mid-flight.
    async fn bridge(&self, client: DuplexStream, upstream: DuplexStream)
    -> Result<(), DomainError>;
}
