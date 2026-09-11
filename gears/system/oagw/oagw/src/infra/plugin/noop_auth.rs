//! Built-in no-op auth plugin.

use async_trait::async_trait;

use crate::domain::plugin::{AuthPlugin, PluginError, RequestContext};

/// Injects nothing; used for upstreams that need no authentication.
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        ctx.record(self.id(), crate::domain::plugin::PluginPhase::Request);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::plugin::gts_helpers;
    use crate::infra::plugin::test_support::{recorded, request_context};

    #[tokio::test]
    async fn the_inbound_headers_are_left_untouched() {
        let mut ctx = request_context("local");
        ctx.headers.insert(
            "authorization",
            http::HeaderValue::from_static("Bearer inbound"),
        );

        NoopAuthPlugin
            .authenticate(&mut ctx)
            .await
            .expect("a no-op never fails");
        assert_eq!(
            ctx.headers.get("authorization").unwrap(),
            "Bearer inbound",
            "nothing is injected or replaced"
        );
        assert_eq!(recorded(&ctx), vec!["noop:Request".to_owned()]);
    }

    #[test]
    fn the_gts_identifier_follows_the_auth_plugin_family() {
        assert_eq!(NoopAuthPlugin.id(), "noop");
        assert_eq!(NoopAuthPlugin.gts_id(), gts_helpers::NOOP_AUTH);
    }
}
