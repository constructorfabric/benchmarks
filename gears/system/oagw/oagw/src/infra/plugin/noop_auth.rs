//! The `noop` auth plugin: a pass-through credential injector.
//!
//! It exists so an upstream can declare an auth chain without credentials and
//! still traverse the single-auth-plugin rule of ADR 0002.

use async_trait::async_trait;

use crate::domain::gts_helpers::BUILTIN_AUTH_NOOP;
use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// The pass-through auth plugin.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuth;

#[async_trait]
impl AuthPlugin for NoopAuth {
    fn id(&self) -> &str {
        BUILTIN_AUTH_NOOP
    }

    fn plugin_type(&self) -> &str {
        crate::domain::gts_helpers::AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_noop_plugin_leaves_the_request_alone() {
        let mut ctx = RequestContext::default();
        ctx.set_header("authorization", "Bearer existing");
        NoopAuth.authenticate(&mut ctx).await.unwrap();
        assert_eq!(ctx.header("authorization"), Some("Bearer existing"));
    }

    #[test]
    fn the_plugin_id_is_the_builtin_identifier() {
        assert_eq!(NoopAuth.id(), BUILTIN_AUTH_NOOP);
    }
}
