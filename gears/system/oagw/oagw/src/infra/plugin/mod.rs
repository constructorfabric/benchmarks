//! Built-in plugin implementations.
//!
//! * [`secret`] — credential resolution through the credstore dependency
//! * [`base64`] — minimal standard base64 encoder (no external dependency)
//! * [`apikey_auth`] — outbound API-key header
//! * [`noop_auth`] — empty credential injector
//! * [`oauth2_client_cred_auth`] — OAuth2 client-credentials grant (ADR-0008),
//!   registered both as `oauth2_client_cred` and as `oauth2_client_cred_basic`
//! * [`required_headers_guard`] — mandatory inbound headers (ADR-0009)
//! * [`request_id_transform`] — correlation identifier injection

/// Credential resolution.
pub mod secret;
/// Minimal standard base64 encoder.
pub mod base64;
/// Built-in `apikey` auth plugin.
pub mod apikey_auth;
/// Built-in `noop` auth plugin.
pub mod noop_auth;
/// Built-in `oauth2_client_credentials` auth plugin.
pub mod oauth2_client_cred_auth;
/// Built-in `required_headers` guard plugin.
pub mod required_headers_guard;
/// Built-in `request_id` transform plugin.
pub mod request_id_transform;
/// Registry assembly.
pub mod registry;

pub use registry::{builtin_registry, builtin_registry_with_credstore};
pub use secret::CredentialSource;
