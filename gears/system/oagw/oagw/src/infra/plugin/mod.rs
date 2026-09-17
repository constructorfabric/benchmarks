//! Built-in plugin implementations (ADR 0002 "Built-in Plugins").
//!
//! Registered in [`registry`] and resolved by GTS instance id. Every built-in
//! plugin has a complete implementation here; the credential-injection ones
//! resolve their material through the data plane's
//! [`PluginRuntime`](crate::infra::proxy::PluginRuntime).
//!
//! Catalog-only identifiers (no backing trait implementation, registered in
//! the types-registry only): `basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`.
//!
//! Catalog-only identifiers (no backing trait implementation, registered in
//! the types-registry only): `basic`, `bearer`, `timeout`, `cors`, `logging`,
//! `metrics`.

pub mod apikey_auth;
pub mod basic_auth;
pub mod bearer_auth;
pub mod cors;
pub mod logging;
pub mod metrics;
pub mod noop;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id;
pub mod required_headers_guard;
pub mod timeout;

pub use apikey_auth::{ApiKeyAuthConfig, ApiKeyAuthPlugin};
pub use basic_auth::BasicAuthPlugin;
pub use bearer_auth::BearerAuthPlugin;
pub use logging::LoggingConfig;
pub use metrics::{labels, names};
pub use noop::NoopAuthPlugin;
pub use oauth2_client_cred_auth::{OAuth2ClientCredAuthConfig, OAuth2ClientCredAuthPlugin};
pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
pub use request_id::RequestIdTransformPlugin;
pub use required_headers_guard::{RequiredHeadersGuardConfig, RequiredHeadersGuardPlugin};
pub use timeout::TimeoutConfig;

/// Error code reported by ADR 0009's required-headers guard.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";
