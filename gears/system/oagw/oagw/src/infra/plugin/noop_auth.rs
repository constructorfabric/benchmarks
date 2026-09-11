//! `noop` auth plugin — the explicit "this upstream needs no credential"
//! binding, so an unauthenticated upstream is a deliberate configuration
//! choice rather than an omission.

use async_trait::async_trait;

use crate::domain::error::PluginError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// `noop` built-in auth plugin.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        gts::NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::NoopAuthPlugin;
    use crate::domain::gts_helpers as gts;
    use crate::domain::plugin::{AuthPlugin, RequestContext};
    use http::{HeaderMap, Method};
    use serde_json::Map;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    #[tokio::test]
    async fn injects_nothing() {
        let mut ctx = RequestContext {
            method: Method::GET,
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            config: Map::new(),
            security_context: SecurityContext::anonymous(),
            alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
        };
        let plugin = NoopAuthPlugin;
        assert_eq!(plugin.plugin_type(), gts::NOOP_AUTH_PLUGIN_ID);
        plugin.authenticate(&mut ctx).await.expect("no-op");
        assert!(ctx.headers.is_empty());
        assert!(ctx.query.is_empty());
    }
}
