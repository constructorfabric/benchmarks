//! The no-op auth plugin: a bindable credential injector that injects nothing.

use super::{AuthDecision, AuthPlugin, PluginRequestContext};
use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use crate::gts_helpers;
use async_trait::async_trait;

/// Built-in auth plugin that performs no credential injection.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        gts_helpers::AUTH_NOOP
    }

    async fn authenticate(
        &self,
        _context: &mut PluginRequestContext,
        _config: &AuthConfig,
        _credentials: &dyn super::CredentialResolver,
    ) -> Result<AuthDecision, OagwError> {
        Ok(AuthDecision::Passthrough)
    }
}

#[cfg(test)]
#[path = "noop_tests.rs"]
mod tests;
