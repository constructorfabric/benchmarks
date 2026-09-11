//! `cf.core.oagw.noop.v1` — the explicit "this upstream needs no
//! credentials" auth plugin.

use async_trait::async_trait;

use crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// Injects nothing. Exists so that "no auth" is a deliberate configuration
/// choice rather than an omission.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;

    #[tokio::test]
    async fn injects_nothing() {
        let mut ctx = request_context(serde_json::Map::new());
        let before = ctx.headers.len();
        NoopAuthPlugin.authenticate(&mut ctx).await.expect("noop");
        assert_eq!(ctx.headers.len(), before);
    }
}
