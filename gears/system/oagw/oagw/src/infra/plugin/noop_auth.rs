//! `noop.v1` — no authentication (DESIGN built-in auth plugin).

use crate::domain::plugin::{AuthContext, AuthPlugin, NOOP_AUTH_PLUGIN_ID, PluginError};

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`
///
/// Injects nothing; the request passes through unchanged. Requires no
/// credentials and ignores `auth.config`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
