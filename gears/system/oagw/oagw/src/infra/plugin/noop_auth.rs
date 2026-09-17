//! `NoopAuthPlugin` — no credential injection (ADR-0002).

use async_trait::async_trait;

use crate::domain::error::OagwResult;
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// Pass-through auth plugin: forwards the request without credentials.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> OagwResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn leaves_headers_untouched() {
        let mut ctx = crate::infra::plugin::test_support::request_context();
        NoopAuthPlugin.authenticate(&mut ctx).await.unwrap();
        assert!(ctx.headers.get("authorization").is_none());
    }
}
