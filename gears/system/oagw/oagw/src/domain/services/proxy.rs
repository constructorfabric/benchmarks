//! Data-plane contract: proxy execution (DESIGN §3.3 "Proxy API").

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{ErrorSource, OagwResult};
use crate::domain::model::{Endpoint, Route, Upstream};

/// How the caller selected the target endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetHostChoice {
    /// Let the data plane pick (round-robin / derivation rules).
    Auto,
    /// `X-OAGW-Target-Host` was supplied (validated against the endpoints).
    Pinned(String),
}

/// Result of alias + endpoint resolution on the control plane.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The upstream that owns the alias.
    pub upstream: Upstream,
    /// Endpoint selected for this request.
    pub endpoint: Endpoint,
    /// Tenant that owns the upstream (may be an ancestor of the caller).
    pub tenant_id: Uuid,
    /// Tenant chain walked during resolution, nearest ancestor first,
    /// ending with the root.
    pub tenant_chain: Vec<Uuid>,
}

impl ResolvedTarget {
    /// Effective upstream id.
    #[must_use]
    pub fn upstream_id(&self) -> Uuid {
        self.upstream.id
    }
}

/// A matched route plus the upstream-bound request target.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    /// Matched route.
    pub route: Route,
    /// Path sent upstream (`match.http.path` plus the appended suffix).
    pub upstream_path: String,
    /// Query parameters forwarded upstream.
    pub query: Vec<(String, String)>,
}

/// Upstream response, streamed to the caller.
#[derive(Debug)]
pub struct UpstreamResponse {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Response headers to forward.
    pub headers: HeaderMap,
    /// Buffered body (plugin phases, non-streaming flows).
    pub body: Bytes,
    /// Streaming body, present when the response is streamed through
    /// (SSE, chunked, or any large body).
    pub stream: Option<crate::infra::proxy::stream::UpstreamBodyStream>,
    /// Side that produced this response (ADR-0007).
    pub source: ErrorSource,
    /// Upgraded connection, present when the upstream accepted a 101.
    pub upgraded: Option<hyper::upgrade::Upgraded>,
}

/// A fully rendered upstream request handed to the transport layer.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// Method to send.
    pub method: String,
    /// Absolute upstream path (starts with `/`).
    pub path: String,
    /// Query parameters forwarded upstream.
    pub query: Vec<(String, String)>,
    /// Headers to send (already transformed).
    pub headers: HeaderMap,
    /// Body to send.
    pub body: Bytes,
    /// `true` when the caller is upgrading (WebSocket).
    pub is_upgrade: bool,
    /// Endpoint to dial.
    pub endpoint: Endpoint,
}

/// Data-plane operations: execute one proxy request end to end.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Executes a proxy request.
    ///
    /// # Errors
    ///
    /// Returns the appropriate [`OagwError`] for validation failures, rate
    /// limiting, routing and upstream failures.
    async fn proxy(&self, request: ProxyRequest) -> OagwResult<UpstreamResponse>;

    /// Handles a CORS preflight for `alias`.
    ///
    /// Preflight never touches the upstream (ADR-0004): browser preflights
    /// carry no credentials, so there is no tenant context to resolve with.
    /// The answer is a permissive `204` echoing the requested origin, method
    /// and headers; enforcement happens on the actual request.
    ///
    /// # Errors
    ///
    /// Never fails; the result type matches the proxy contract.
    async fn preflight(&self, request: ProxyRequest) -> OagwResult<http::Response<Bytes>>;
}

/// A proxy request as received by the REST handler.
#[derive(Debug)]
pub struct ProxyRequest {
    /// Authenticated caller.
    pub security_context: SecurityContext,
    /// Alias from the URL path.
    pub alias: String,
    /// Path suffix after `{alias}` (empty when absent).
    pub path_suffix: String,
    /// Query parameters as `(name, value)` pairs.
    pub query: Vec<(String, String)>,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Inbound method.
    pub method: http::Method,
    /// Inbound body.
    pub body: Bytes,
    /// `X-OAGW-Target-Host` selection, if provided.
    pub target_host: TargetHostChoice,
    /// Client WebSocket upgrade handle, when the request upgrades.
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    /// URI used for the `instance` member of problem documents.
    pub instance: String,
}

impl ProxyRequest {
    /// `true` when the client asked for a WebSocket upgrade.
    #[must_use]
    pub fn is_upgrade(&self) -> bool {
        self.on_upgrade.is_some()
    }
}
