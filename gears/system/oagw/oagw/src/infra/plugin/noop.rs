//! No-op auth plugin (`...~cf.core.oagw.noop.v1`).

use async_trait::async_trait;

use crate::domain::models::plugin_gts::AUTH_NOOP;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// No-op auth plugin — performs no authentication.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
