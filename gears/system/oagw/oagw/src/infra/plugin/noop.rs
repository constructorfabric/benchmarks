//! No-op auth plugin (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`).
//!
//! Declares "no upstream authentication": the outbound request is forwarded
//! exactly as the data plane built it. Registering it is what makes an
//! upstream with no `auth` block behave identically to one that explicitly
//! binds `noop`.

use async_trait::async_trait;

use crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// The built-in no-op auth plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
        // No credentials to inject: the request is forwarded untouched.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_untouched() {
        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body: None,
            downstream_headers: http::HeaderMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            route_id: None,
            alias: None,
            trace_id: None,
            config: crate::domain::plugin::PluginConfig::default(),
            attributes: Default::default(),
        };
        NoopAuthPlugin.authenticate(&mut ctx).await.unwrap();
        assert!(ctx.headers.is_empty());
    }
}
