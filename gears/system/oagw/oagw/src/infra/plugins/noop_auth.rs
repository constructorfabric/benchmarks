//! `noop` auth plugin — injects nothing (`PRD.md` § 5.3).

use async_trait::async_trait;

use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// The no-op credential injector.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::gts_helpers::AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_the_documented_gts_id() {
        assert_eq!(NoopAuthPlugin.id(), crate::domain::gts_helpers::AUTH_NOOP);
        assert_eq!(NoopAuthPlugin.plugin_type(), "auth");
    }
}
