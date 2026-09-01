//! The no-op auth plugin (`DESIGN` §3.2).
//!
//! Upstreams that authenticate by some out-of-band mechanism — an allow-listed
//! source address, mTLS termination in front of the gateway — declare
//! `auth.type: noop` so the proxy forwards the caller's credentials untouched.

use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthOutcome, AuthPlugin};

/// An auth plugin that injects nothing and forwards the caller's credentials.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

impl NoopAuthPlugin {
    /// The plugin instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn gts_id(&self) -> String {
        NOOP_AUTH_PLUGIN_ID.to_owned()
    }

    async fn authenticate(&self, _request: &mut ProxyContext) -> Result<AuthOutcome, DomainError> {
        Ok(AuthOutcome::default())
    }
}
