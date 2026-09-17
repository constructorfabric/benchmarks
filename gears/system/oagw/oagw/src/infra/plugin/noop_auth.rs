//! `noop` auth plugin: no credential injection (ADR-0002).

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Auth plugin that leaves the request untouched.
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        crate::ids::AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}
