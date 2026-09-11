//! Built-in `noop` auth plugin: injects nothing.

use serde_json::Value;

use crate::domain::gts_helpers;
use crate::domain::plugin::{AuthPlugin, PluginError, ProxyRequest};

/// The no-op authentication plugin.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::AUTH_NOOP
    }

    fn plugin_type(&self) -> &'static str {
        "noop"
    }

    async fn authenticate(
        &self,
        _request: &mut ProxyRequest,
        _config: &Value,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "noop_auth_tests.rs"]
mod tests;
