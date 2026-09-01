//! Data Plane service contract.
//!
//! The Data Plane owns the proxy request lifecycle: config resolution (via the
//! Control Plane), plugin execution, the outbound HTTP call and metric
//! emission. Request and response bodies are streamed, never buffered.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// Body type used for proxy request / response payloads.
pub type ProxyBody = axum::body::Body;

/// A proxy request as handed to the Data Plane by the transport layer.
///
/// The URI path is the *upstream-relative* path (after the alias and the
/// matched route prefix), the query string is the raw client query, and the
/// headers are the inbound headers minus routing and hop-by-hop headers.
#[derive(Debug)]
pub struct ProxyRequest {
    /// HTTP method of the inbound request.
    pub method: http::Method,
    /// Upstream-relative path (starts with `/`).
    pub path: String,
    /// Raw query string (without `?`); empty when absent.
    pub query: String,
    /// Inbound headers (already stripped of routing/hop-by-hop headers).
    pub headers: http::HeaderMap,
    /// Inbound request body stream.
    pub body: ProxyBody,
    /// `true` when the inbound request carries an `Upgrade` token that must be
    /// relayed verbatim (WebSocket / WebTransport).
    pub is_upgrade: bool,
}

impl ProxyRequest {
    /// The full request URI (`path` plus `?query` when present).
    #[must_use]
    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        }
    }
}

/// Owned request metadata, borrowable across `await` points.
///
/// [`ProxyRequest`] carries a streaming body that is neither `Send`-friendly
/// to share, so the Data Plane splits every request into this metadata bundle
/// and the body stream before it starts awaiting.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    /// HTTP method of the inbound request.
    pub method: http::Method,
    /// Upstream-relative path (starts with `/`).
    pub path: String,
    /// Raw query string (without `?`); empty when absent.
    pub query: String,
    /// Inbound headers (already stripped of routing and hop-by-hop headers).
    pub headers: http::HeaderMap,
    /// `true` when the inbound request carries an `Upgrade` token.
    pub is_upgrade: bool,
}

impl RequestMeta {
    /// Builds metadata from its parts.
    #[must_use]
    pub fn new(
        method: http::Method,
        path: String,
        query: String,
        headers: http::HeaderMap,
        is_upgrade: bool,
    ) -> Self {
        Self {
            method,
            path,
            query,
            headers,
            is_upgrade,
        }
    }
}

impl ProxyRequest {
    /// Splits the request into owned metadata and the body stream.
    pub fn into_parts(mut self) -> (RequestMeta, ProxyBody) {
        let body = std::mem::replace(&mut self.body, ProxyBody::empty());
        let meta = RequestMeta {
            method: self.method,
            path: self.path,
            query: self.query,
            headers: self.headers,
            is_upgrade: self.is_upgrade,
        };
        (meta, body)
    }
}

/// A relayed upstream response, with `X-OAGW-Error-Source` applied.
#[derive(Debug)]
pub struct ProxyOutcome {
    /// Upstream response status.
    pub status: http::StatusCode,
    /// Upstream response headers plus gateway-added headers.
    pub headers: http::HeaderMap,
    /// Upstream response body stream.
    pub body: ProxyBody,
}

/// Data Plane service contract.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Executes a proxy request end to end.
    ///
    /// The returned response always carries `X-OAGW-Error-Source: upstream`;
    /// gateway failures are returned as [`DomainError`] and mapped to
    /// `X-OAGW-Error-Source: gateway` problems by the transport layer.
    ///
    /// # Errors
    ///
    /// Routing, rate-limit, guard, auth, protocol and timeout failures.
    async fn proxy(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        request: ProxyRequest,
    ) -> Result<ProxyOutcome, DomainError>;
}
