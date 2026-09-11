//! `cf.core.oagw.noop.v1` — no authentication.

use async_trait::async_trait;

use crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};

/// Injects nothing. Present so an upstream can state explicitly that it needs
/// no outbound credential, rather than leaving `auth` unset by accident.
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

    async fn authenticate(&self, _ctx: &mut AuthContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PluginConfig;
    use axum::http::HeaderMap;
    use toolkit_security::SecurityContext;

    #[tokio::test]
    async fn injects_nothing() {
        let mut ctx = AuthContext {
            security_context: SecurityContext::anonymous(),
            config: PluginConfig::new(),
            headers: HeaderMap::new(),
            query: vec![],
            upstream_alias: "api.example.com".to_owned(),
        };
        NoopAuthPlugin.authenticate(&mut ctx).await.expect("noop");
        assert!(ctx.headers.is_empty());
        assert!(ctx.query.is_empty());
    }
}
