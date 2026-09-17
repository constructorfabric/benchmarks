//! Data-plane service contract: proxies a client request to an upstream.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::ProblemSpec;

/// Input assembled by the proxy handler.
pub struct ProxyInput {
    /// Normalized alias taken from the URL path.
    pub alias: String,
    /// Optional `/path_suffix` from the proxy URL (empty when absent).
    pub path_suffix: String,
    /// The caller's peer address (used for `ip`-scoped rate limits and
    /// `X-Forwarded-For`), when known.
    pub client_ip: Option<std::net::IpAddr>,
    /// The full inbound request (headers + streaming body).
    pub request: http::Request<axum::body::Body>,
}

/// Data-plane service: alias resolution → route matching → config merge →
/// plugin chain → forward → transform response.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Execute a proxied request.
    ///
    /// # Errors
    ///
    /// Returns a full gateway [`ProblemSpec`] (RFC 9457 problem+json with
    /// `X-OAGW-Error-Source: gateway`) for every gateway-originated failure.
    async fn execute_proxy(
        &self,
        security_ctx: SecurityContext,
        input: ProxyInput,
    ) -> Result<http::Response<axum::body::Body>, ProblemSpec>;
}
