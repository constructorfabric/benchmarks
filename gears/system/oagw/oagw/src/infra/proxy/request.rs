//! The proxy request, normalised from the transport layer.
//!
//! The API handler owns the axum types; the data plane speaks this shape, which
//! carries no transport dependency beyond the body it must hand to hyper.
use http::HeaderMap;
use uuid::Uuid;

use crate::infra::proxy::cors::CorsRequest;

/// A request the proxy is about to execute.
#[derive(Debug)]
pub struct ProxyRequest {
    /// Tenant of the caller.
    pub tenant_id: Uuid,
    /// Calling principal.
    pub subject_id: String,
    /// Subject tenant as the platform established it.
    pub subject_tenant_id: String,
    /// Alias the request addressed.
    pub alias: String,
    /// Upstream-relative path, always starting with `/`.
    pub path: String,
    /// Raw query string, without the leading `?`.
    pub query: Option<String>,
    /// Request method, upper-case.
    pub method: String,
    /// Inbound headers, untouched.
    pub headers: HeaderMap,
    /// Inbound body, streamed.
    pub body: Option<axum::body::Body>,
    /// Whether the client asked for a protocol upgrade.
    pub upgrade: bool,
    /// `X-OAGW-Target-Host`, when the client supplied one.
    pub target_host: Option<String>,
    /// What CORS needs to know about the request.
    pub cors: CorsRequest,
}

impl ProxyRequest {
    /// Whether the request is a CORS preflight.
    #[must_use]
    pub fn is_preflight(&self) -> bool {
        crate::infra::proxy::cors::is_preflight(&self.cors)
    }
}
