// Updated: 2026-09-01 by Constructor Tech
//! `noop` auth plugin: no authentication at all.
//!
//! Present so an operator can state "this upstream needs no upstream-side
//! credentials" explicitly, instead of omitting `auth` and leaving the intent
//! ambiguous in the configuration.

use async_trait::async_trait;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// The no-op auth plugin.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &'static str {
        crate::gts::AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticate_is_a_no_op() {
        let mut ctx = crate::infra::plugin::test_support::request_context();
        NoopAuthPlugin
            .authenticate(&mut ctx)
            .await
            .expect("noop must always succeed");
    }
}
