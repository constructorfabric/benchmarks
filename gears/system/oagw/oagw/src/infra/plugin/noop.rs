//! Built-in auth no-op plugin (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`).
//!
//! The default auth binding of an upstream that needs no outbound credential:
//! it changes nothing about the request and never fails, which keeps the
//! ADR-0002 execution order (auth -> guards -> transform) intact for upstreams
//! that would otherwise have no auth plugin at all.

use async_trait::async_trait;

use crate::domain::error::OagwError;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};

/// Auth no-op plugin.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

impl NoopAuthPlugin {
    /// Registry key of this plugin.
    pub const PLUGIN_ID: &'static str = builtin::NOOP_AUTH;

    /// GTS base type of this plugin.
    pub const PLUGIN_TYPE: &'static str = AUTH_PLUGIN_TYPE_ID;
}

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        Self::PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        Self::PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "noop_tests.rs"]
mod tests;
