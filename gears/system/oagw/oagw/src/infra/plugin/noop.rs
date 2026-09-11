// Created: 2026-09-01 by Constructor Tech
//! The no-op auth plugin.

use async_trait::async_trait;

use crate::domain::errors::OagwError;
use crate::domain::model::builtin_plugins;
use crate::infra::context::PluginRequest;
use crate::infra::plugin::traits::AuthPlugin;

/// Injects nothing. The default when an upstream declares no `auth` block.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        builtin_plugins::AUTH_NOOP
    }

    async fn authenticate(&self, _request: &mut PluginRequest) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_leaves_the_request_untouched() {
        let mut request = PluginRequest {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            target: crate::domain::model::Target {
                host: "h".to_owned(),
                port: 443,
                secure: true,
            },
            alias: "h".to_owned(),
            upstream_id: "u".to_owned(),
            route_id: None,
            tenant_id: "t".to_owned(),
            subject: None,
            request_id: "r".to_owned(),
            content_type: None,
            auth_config: std::collections::BTreeMap::new(),
            plugin_config: std::collections::BTreeMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
        };
        NoopAuthPlugin
            .authenticate(&mut request)
            .await
            .expect("no-op");
        assert!(request.headers.is_empty());
    }
}
