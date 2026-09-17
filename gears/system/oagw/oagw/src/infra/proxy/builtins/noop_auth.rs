//! The `noop` auth plugin: no credential injection (ADR-0002).
//!
//! Exists so an upstream can state "this upstream needs no credentials"
//! explicitly instead of leaving `auth` empty.

use async_trait::async_trait;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Auth plugin that forwards the request untouched.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

const INSTANCE: &str = "cf.core.oagw.noop.v1";
const PLUGIN_TYPE: &str = "cf.core.oagw.auth_plugin.v1";

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        INSTANCE
    }

    fn plugin_type(&self) -> &str {
        PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        ctx.set_attribute("oagw.auth.plugin", INSTANCE);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use uuid::Uuid;

    fn ctx() -> RequestContext {
        RequestContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: String::new(),
            headers: axum::http::HeaderMap::new(),
            body: Bytes::new(),
            config: serde_json::Value::Null,
            attributes: Default::default(),
        }
    }

    #[tokio::test]
    async fn leaves_the_request_untouched() {
        let mut ctx = ctx();
        NoopAuthPlugin.authenticate(&mut ctx).await.expect("noop");
        assert_eq!(
            ctx.attribute("oagw.auth.plugin"),
            Some("cf.core.oagw.noop.v1")
        );
        assert!(ctx.headers.is_empty());
    }
}
