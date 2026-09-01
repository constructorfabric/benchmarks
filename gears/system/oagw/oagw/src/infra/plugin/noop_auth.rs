//! `noop` auth plugin — no authentication performed.

use crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext, async_trait};

/// The `noop` auth plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait declares `&str`; returns a literal
    fn plugin_type(&self) -> &str {
        "noop"
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
        Ok(())
    }
}
