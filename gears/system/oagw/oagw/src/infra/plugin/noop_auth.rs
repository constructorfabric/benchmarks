//! The `noop` auth plugin: performs no credential injection.
//!
//! It exists so an upstream with no `auth_methods` still has a chain entry to
//! run, and so operators can bind an explicit no-op when a route inherits an
//! upstream that does authenticate.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginType, RequestContext};
use crate::infra::plugin::registry::PluginFactory;

/// Authenticates by doing nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoopAuth;

#[async_trait]
impl AuthPlugin for NoopAuth {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &'static str {
        "auth"
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Builds `noop` instances; the configuration is ignored.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthFactory;

impl PluginFactory<dyn AuthPlugin> for NoopAuthFactory {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    fn description(&self) -> &'static str {
        "Authenticate nothing: the caller is trusted as presented"
    }

    fn create(
        &self,
        _config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn AuthPlugin>, DomainError> {
        Ok(Arc::new(NoopAuth))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod noop_auth_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn authenticate_injects_nothing() {
        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body_present: false,
            security_context: toolkit_security::SecurityContext::anonymous(),
            tenant_scope: vec![uuid::Uuid::nil()],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        };
        let empty = serde_json::Map::new();
        let plugin: Arc<dyn AuthPlugin> = NoopAuthFactory.create(&empty).unwrap();
        plugin.authenticate(&mut ctx).await.unwrap();
        assert!(ctx.injected_headers.is_empty());
        assert_eq!(plugin.id(), "noop");
    }
}
