//! The no-op auth plugin (ADR-0002 "Built-in Plugins": `NoopAuthPlugin`).
//!
//! It exists so an upstream can declare "this binding is deliberate" —
//! `auth.type: ...noop.v1` — instead of leaving `auth` unset, which reads as
//! "not configured yet". It writes nothing.

use http::HeaderMap;

use super::registry::NOOP_AUTH_PLUGIN_REF;
use super::traits::{AuthPlugin, PluginContext};
use crate::error::OagwError;

/// The plugin that authenticates nobody, on purpose.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait::async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn plugin_ref(&self) -> &str {
        NOOP_AUTH_PLUGIN_REF
    }

    async fn authenticate(
        &self,
        _context: &PluginContext<'_>,
        _headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn the_noop_plugin_writes_nothing_and_never_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "caller-sent".parse().expect("valid"));

        let context = PluginContext {
            config: Some(&json!({"header": "authorization"})),
            request: &test_context(),
        };

        NoopAuthPlugin
            .authenticate(&context, &mut headers)
            .await
            .expect("a no-op never fails");

        assert_eq!(
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("caller-sent"),
            "the plugin does not touch the outbound headers"
        );
    }

    /// A minimal [`ProxyContext`](crate::domain::services::data_plane::ProxyContext)
    /// for the plugin tests.
    fn test_context() -> crate::domain::services::data_plane::ProxyContext {
        use crate::domain::services::data_plane::ProxyContext;
        use crate::domain::types::{Endpoint, Scheme, ServerConfig, Upstream, UpstreamSpec};
        use std::sync::Arc;
        use uuid::Uuid;

        ProxyContext {
            subject_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            route_id: None,
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            request_id: "01JREQUESTID".to_owned(),
            security: toolkit_security::SecurityContext::anonymous(),
            client_ip: None,
            upstream: Arc::new(Upstream {
                id: Uuid::new_v4(),
                tenant_id: Uuid::new_v4(),
                alias: "api.vendor.com".to_owned(),
                created_at: 0,
                updated_at: 0,
                spec: UpstreamSpec {
                    alias: Some("api.vendor.com".to_owned()),
                    server: ServerConfig {
                        endpoints: vec![Endpoint {
                            scheme: Scheme::Http,
                            host: "127.0.0.1".to_owned(),
                            port: 8080,
                        }],
                    },
                    ..UpstreamSpec::default()
                },
            }),
            route: None,
            inbound_headers: HeaderMap::new(),
        }
    }
}
