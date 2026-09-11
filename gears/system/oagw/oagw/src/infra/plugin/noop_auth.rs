//! `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` — no authentication.

use async_trait::async_trait;

use crate::domain::gts_helpers::{AUTH_PLUGIN_TYPE, NOOP_AUTH_PLUGIN_ID};
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// Injects nothing. Present so an upstream can declare "no credentials" as an
/// explicit choice rather than an omission.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
        Ok(())
    }
}

/// The base type this plugin implements.
#[must_use]
pub fn base_type() -> &'static str {
    AUTH_PLUGIN_TYPE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_its_gts_identifier() {
        let plugin = NoopAuthPlugin;
        assert_eq!(plugin.plugin_type(), NOOP_AUTH_PLUGIN_ID);
        assert!(plugin.plugin_type().starts_with(base_type()));
    }
}
