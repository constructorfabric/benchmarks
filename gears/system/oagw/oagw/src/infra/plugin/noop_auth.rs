//! `NoopAuthPlugin` — identity auth for upstreams that need no credentials.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::gts::AUTH_PLUGIN_NOOP_INSTANCE;
use crate::domain::plugin::{AuthPlugin, PluginContext};

/// Injects nothing; used when an upstream is explicitly unauthenticated.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        AUTH_PLUGIN_NOOP_INSTANCE
    }

    async fn authenticate(
        &self,
        _ctx: &PluginContext,
        _security_context: &SecurityContext,
        _config: &serde_json::Value,
        _parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn noop_leaves_the_request_untouched() {
        let ctx = PluginContext {
            security_context: SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            host: "vendor.com".to_owned(),
            route_id: None,
            endpoint_host: "api.vendor.com:443".to_owned(),
            request_id: PluginContext::default_request_id(),
        };
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        NoopAuthPlugin
            .authenticate(
                &ctx,
                &SecurityContext::anonymous(),
                &serde_json::json!({}),
                &mut parts,
            )
            .await
            .unwrap();
        assert!(parts.headers.is_empty());
        assert_eq!(ctx.host, "vendor.com");
        assert_eq!(NoopAuthPlugin.id(), "cf.core.oagw.noop.v1");
    }
}
