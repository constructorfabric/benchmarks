//! `DataPlaneService` — the proxy-path seam of entry 2.4
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`).
//!
//! The handler of `/oagw/v1/proxy/{alias}[/{path_suffix}]` dispatches every
//! non-preflight request to this trait and renders what comes back. The trait
//! is the boundary the later entries cross: the plugin chain (2.6), the
//! rate-limit and CORS call-ins (2.7/2.8) and the L1 hot-config cache (2.9)
//! all consume or replace parts of what [`DataPlaneService::proxy`] resolves,
//! without the handler ever changing.
//!
//! The trait is deliberately *not* the one
//! [`crate::domain::services::ControlPlaneService::resolve_proxy_target`] of
//! entry 2.1: that signature is the ADR 0006 single-tenant shape, while the
//! proxy path walks the tenant chain, selects the endpoint, validates the
//! request surface and carries the error-source attribution.

use async_trait::async_trait;

use crate::domain::proxy::{ProxyContext, ProxyFailure, ProxyResponse};

/// The data plane the proxy handler dispatches to.
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Run one proxied exchange.
    ///
    /// # Errors
    ///
    /// Returns the [`ProxyFailure`] the handler renders: a
    /// [`ProxyFailure::Domain`] for every rejection of the shared error table,
    /// and [`ProxyFailure::MethodNotAllowed`] for a path-matched route whose
    /// method allowlist excludes the request method.
    async fn proxy(&self, context: ProxyContext) -> Result<ProxyResponse, ProxyFailure>;
}
