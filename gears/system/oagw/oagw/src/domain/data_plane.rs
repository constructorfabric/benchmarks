//! Data plane contract: buffered request in, streamed response out.

use async_trait::async_trait;
use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use std::net::IpAddr;

use super::control_plane::Caller;
use crate::error::{ErrorContext, OagwError};

/// Body size ceiling for proxied requests (DESIGN.md: max 100 MiB,
/// rejected before buffering).
pub const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;

/// A gateway error with the request context needed to render the RFC 9457
/// problem body (upstream id, host, path, alias).
#[derive(Debug)]
pub struct ProxyFailure {
    pub error: OagwError,
    pub ctx: ErrorContext,
}

impl ProxyFailure {
    #[must_use]
    pub fn new(error: OagwError, ctx: ErrorContext) -> Self {
        Self { error, ctx }
    }
}

/// A fully-buffered inbound request to forward.
#[derive(Debug)]
pub struct ProxyRequest {
    pub method: Method,
    /// Path after the alias (decoded; may be empty).
    pub path_suffix: String,
    /// Raw query string (may be `None`).
    pub raw_query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub client_ip: Option<IpAddr>,
}

/// The proxied upstream response with a streamed body.
#[derive(Debug)]
pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Toolkit-HTTP boxed body, forwarded to the caller as-is.
    pub body: toolkit_http::ResponseBody,
}

/// The data plane: executes the full proxy flow (alias resolution, CORS,
/// auth, guards, transforms, rate limiting, forwarding, response
/// transformation).
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    async fn proxy_request(
        &self,
        caller: &Caller,
        alias: &str,
        request: ProxyRequest,
    ) -> Result<ProxyResponse, ProxyFailure>;
}
