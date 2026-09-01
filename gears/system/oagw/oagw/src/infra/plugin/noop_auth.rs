// Created: 2026-08-31 by Constructor Tech
//! `NoopAuthPlugin` (ADR-0002 "Built-in Plugins").
//!
//! The identity auth binding: an upstream without credentials binds `noop` and
//! the gateway forwards the request untouched. It exists so a record always has
//! a resolvable auth plugin — the data plane never forwards a request for an
//! auth binding it cannot resolve.

use async_trait::async_trait;

use crate::error::OagwError;
use crate::infra::plugin::traits::{AuthPlugin, RequestContext};

/// GTS id of the built-in no-op auth plugin.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";

/// Always succeeds, injects nothing.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &'static str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_is_the_documented_built_in() {
        let plugin = NoopAuthPlugin;
        assert_eq!(plugin.id(), "noop");
        assert_eq!(plugin.plugin_type(), NOOP_AUTH_PLUGIN_ID);
        assert_eq!(
            plugin.plugin_type(),
            crate::domain::plugin::PluginKind::Auth.built_in_id("noop")
        );
    }
}
