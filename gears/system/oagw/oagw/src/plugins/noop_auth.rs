//! `NoopAuthPlugin` — no authentication.

use async_trait::async_trait;

use super::{AuthPlugin, PluginError, RequestContext};

/// Auth plugin that injects nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        crate::gts::auth_plugin::NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}
