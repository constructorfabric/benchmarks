//! `noop` auth plugin — injects nothing (PRD §5.3).

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, PluginConfig, RequestContext};

/// No-authentication plugin.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        gts::AUTH_NOOP
    }

    async fn authenticate(
        &self,
        _ctx: &mut RequestContext,
        _config: &PluginConfig,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}
