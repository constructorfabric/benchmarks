//! The plugin system of the proxy (ADR-0002): the three traits, the in-process
//! registries, the built-in plugins and the engine that runs a request's chain.
//!
//! ```text
//! registry.rs   AuthPluginRegistry / GuardPluginRegistry / TransformPluginRegistry
//! traits.rs     the three plugin traits and the context they are handed
//! engine.rs     chain resolution + execution (auth → guards → transforms)
//! api_key_auth.rs, noop_auth.rs, oauth2_client_cred_auth.rs,
//!               required_headers_guard.rs, request_id_transform.rs
//! secret.rs     `cred://` resolution (DESIGN §2.1 credential isolation)
//! ```
//!
//! Plugins are *not* part of the policy layer of
//! [`crate::domain::policy`]: CORS and rate limiting are core data-plane
//! behaviour by decision of ADR-0002 and ADR-0004, so neither is reachable
//! through a registry here.
//!
//! # What this slice does not implement
//!
//! * **The Starlark sandbox** does not exist yet, so a *custom* (tenant-defined,
//!   UUID-backed) plugin is resolved, named, and rejected with 503 rather than
//!   silently skipped. The seam is [`PluginRegistries`], which an external gear
//!   registers real implementations into.
//! * **Metrics and logging** are core data-plane instrumentation, not
//!   transform plugins (DESIGN §3.1).

pub mod api_key_auth;
pub mod engine;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod secret;
pub mod traits;

pub use api_key_auth::{ApiKeyAuthPlugin, DEFAULT_KEY_HEADER};
pub use engine::PluginEngineService;
pub use noop_auth::NoopAuthPlugin;
pub use oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
pub use registry::{
    API_KEY_AUTH_PLUGIN_REF, AuthPluginRegistry, GuardPluginRegistry, NOOP_AUTH_PLUGIN_REF,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF, PluginRegistries,
    REQUEST_ID_TRANSFORM_PLUGIN_REF, REQUIRED_HEADERS_GUARD_PLUGIN_REF, TransformPluginRegistry,
};
pub use request_id_transform::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
pub use required_headers_guard::{REQUIRED_HEADER_MISSING, RequiredHeadersGuardPlugin};
pub use secret::SecretResolver;
pub use traits::{AuthPlugin, GuardPlugin, PluginContext, TransformPlugin};

/// A [`ProxyContext`](crate::domain::services::data_plane::ProxyContext) for the
/// plugin unit tests.
#[cfg(test)]
pub(crate) fn test_context() -> crate::domain::services::data_plane::ProxyContext {
    use crate::domain::services::data_plane::ProxyContext;
    use crate::domain::types::{Endpoint, Scheme, ServerConfig, Upstream, UpstreamSpec};
    use http::HeaderMap;
    use std::sync::Arc;
    use uuid::Uuid;

    let alias = "api.vendor.com";

    ProxyContext {
        subject_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id: Uuid::new_v4(),
        alias: alias.to_owned(),
        route_id: None,
        method: "GET".to_owned(),
        path: "/v1".to_owned(),
        request_id: "01JREQUESTID".to_owned(),
        security: toolkit_security::SecurityContext::anonymous(),
        client_ip: None,
        upstream: Arc::new(Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: UpstreamSpec {
                alias: Some(alias.to_owned()),
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
