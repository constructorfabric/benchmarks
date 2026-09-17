//! `NoopAuthPlugin` — no authentication.

use async_trait::async_trait;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext, SecretResolver};

/// Identity plugin: forwards the request untouched.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(
        &self,
        _ctx: &mut RequestContext,
        _secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}
