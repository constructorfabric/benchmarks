// Created: 2026-09-01 by Constructor Tech
//! Plugin trait definitions.
//!
//! `docs/ADR/0002-plugin-system.md`. Each phase receives the mutable
//! request state and either allows it through or rejects it. Plugins are
//! stateless: any cache is the plugin's own internal concern.

use async_trait::async_trait;

use crate::domain::errors::OagwError;
use crate::infra::context::{PluginRequest, PluginResponse};

/// The phase a plugin declares support for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // `On*` is the phase vocabulary ADR 0002 uses
pub enum Phase {
    /// Mutating the outbound request.
    OnRequest,
    /// Mutating the inbound response.
    OnResponse,
    /// Producing an error response.
    OnError,
}

/// Credential injection. Exactly one per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Inject credentials into `request`.
    ///
    /// # Errors
    /// Returns an error carrying the documented status; a failed credential
    /// lookup surfaces as `401 AuthenticationFailed`.
    async fn authenticate(&self, request: &mut PluginRequest) -> Result<(), OagwError>;
}

/// Validation and policy enforcement. Many per upstream/route; may reject.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Inspect the outbound request.
    ///
    /// # Errors
    /// Rejects with the phase-specific status.
    async fn guard_request(&self, request: &mut PluginRequest) -> Result<(), OagwError>;

    /// Inspect the upstream response.
    ///
    /// # Errors
    /// Rejects with the phase-specific status.
    async fn guard_response(&self, response: &mut PluginResponse) -> Result<(), OagwError>;
}

/// Request/response mutation. Many per upstream/route, executed in order.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Which phases the plugin participates in.
    fn phases(&self) -> &'static [Phase];

    /// Mutate the outbound request.
    ///
    /// # Errors
    /// Propagated to the caller as a gateway error.
    async fn on_request(&self, request: &mut PluginRequest) -> Result<(), OagwError>;

    /// Mutate the inbound response.
    ///
    /// # Errors
    /// Propagated to the caller as a gateway error.
    async fn on_response(&self, response: &mut PluginResponse) -> Result<(), OagwError>;

    /// Produce an error response.
    ///
    /// # Errors
    /// Propagated to the caller as a gateway error.
    async fn on_error(&self, error: &mut OagwError) -> Result<(), OagwError>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn header_presence_is_case_insensitive() {
        let response = PluginResponse {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            plugin_config: BTreeMap::new(),
        };
        assert!(response.has_header("content-type"));
        assert!(response.has_header("CONTENT-TYPE"));
        assert!(!response.has_header("x-absent"));
    }
}
