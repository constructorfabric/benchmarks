//! Domain services: the Control Plane (configuration ownership) and the
//! Data Plane contract (proxy orchestration).

pub mod management;
pub mod merge;
pub mod odata;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::Request;
use toolkit_security::SecurityContext;

use crate::domain::error::OagwResult;

/// Orchestrates one proxied request end to end (ADR-0001).
///
/// The whole inbound request is handed over so the implementation can decide
/// between buffering, streaming and protocol upgrade without the transport
/// layer having to know which applies.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// # Errors
    /// Returns the gateway error to render as RFC 9457 Problem Details.
    async fn execute_proxy(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        request: Request<Body>,
    ) -> OagwResult<Response>;
}
