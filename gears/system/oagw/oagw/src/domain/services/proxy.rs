//! Data Plane contract.
//!
//! The Data Plane owns one request at a time: it resolves configuration
//! through the Control Plane, runs the plugin chain, opens the upstream
//! connection and streams the exchange back. Because the exchange can be a
//! plain response, an SSE stream or a protocol upgrade, the trait works in
//! terms of `http` types rather than a buffered DTO.

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderMap, Method};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainResult;

/// Header carrying the caller's chosen endpoint within a multi-endpoint pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header distinguishing gateway errors from upstream errors (ADR 0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// One inbound proxy request, already split out of the Axum request.
pub struct ProxyRequest {
    /// Caller identity.
    pub security_context: SecurityContext,
    /// Upstream alias from the proxy URL.
    pub alias: String,
    /// Everything after `{alias}` in the proxy URL, if any.
    pub path_suffix: Option<String>,
    /// Inbound HTTP method, forwarded verbatim.
    pub method: Method,
    /// Raw query string, without the leading `?`.
    pub query: Option<String>,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Inbound body, streamed.
    pub body: Body,
    /// Upgrade handle, present when the client asked for one.
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    /// Client address, for `scope: ip` rate limiting.
    pub client_ip: Option<String>,
    /// Absolute request path, used as the RFC 9457 `instance`.
    pub instance: String,
}

impl std::fmt::Debug for ProxyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Headers and body are deliberately omitted: they can carry
        // credentials, and `cpt-cf-oagw-nfr-credential-isolation` forbids
        // any path that could log them.
        f.debug_struct("ProxyRequest")
            .field("alias", &self.alias)
            .field("path_suffix", &self.path_suffix)
            .field("method", &self.method)
            .field("upgrade", &self.on_upgrade.is_some())
            .finish_non_exhaustive()
    }
}

/// Outcome of a proxied exchange, ready to hand back to Axum.
pub type ProxyResponse = Response;

/// Proxy orchestration.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Execute one proxy request end to end.
    ///
    /// # Errors
    ///
    /// Any [`crate::domain::DomainError`] the resolution, plugin chain or
    /// upstream exchange produces. Upstream *responses* — including 5xx —
    /// are not errors: they are returned as-is with
    /// `X-OAGW-Error-Source: upstream`.
    async fn execute(&self, request: ProxyRequest) -> DomainResult<ProxyResponse>;
}
