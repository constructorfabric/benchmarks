//! `cf.core.oagw.noop.v1` — an upstream that needs no credential.

use async_trait::async_trait;

use crate::domain::gts;
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};

/// Injects nothing. Present so "no auth" is an explicit, auditable choice
/// rather than an omitted field.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        gts::NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
