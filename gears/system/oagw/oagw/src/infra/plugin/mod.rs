//! Built-in plugins and the registries that resolve them.

pub mod apikey_auth;
pub mod credstore;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use apikey_auth::ApiKeyAuthPlugin;
pub use noop_auth::NoopAuthPlugin;
pub use oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
pub use request_id_transform::RequestIdTransformPlugin;
pub use required_headers_guard::RequiredHeadersGuardPlugin;

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the plugin unit tests.

    use std::sync::Arc;

    use credstore_sdk::CredStoreClientV1;
    use serde_json::{Map, Value};
    use toolkit_security::SecurityContext;

    use crate::domain::plugin::{HeaderBag, RequestContext, ResponseContext};

    /// Credstore double seeded with `(reference, value)` pairs.
    pub fn mock_credstore(pairs: Vec<(&str, &str)>) -> Arc<dyn CredStoreClientV1> {
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        ))
    }

    /// Minimal request context with a deterministic security context.
    pub fn request_context(config: Map<String, Value>) -> RequestContext {
        RequestContext {
            security_context: SecurityContext::builder()
                .subject_id(uuid::Uuid::nil())
                .subject_tenant_id(uuid::Uuid::nil())
                .build()
                .unwrap_or_else(|_| SecurityContext::anonymous()),
            config,
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers: HeaderBag::new(),
            body: None,
            upstream_alias: "api.example.com".to_owned(),
            upstream_host: "api.example.com".to_owned(),
        }
    }

    /// Minimal response context.
    pub fn response_context(config: Map<String, Value>) -> ResponseContext {
        ResponseContext {
            config,
            status: 200,
            headers: HeaderBag::new(),
            body: None,
        }
    }
}
