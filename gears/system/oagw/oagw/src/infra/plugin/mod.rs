//! Plugin implementations.

pub mod builtin;
pub mod credential;

pub use builtin::{NoopAuthPlugin, REQUEST_ID_HEADER, register_builtins};
pub use credential::{
    ApiKeyAuthPlugin, CredentialStore, OAuth2ClientCredAuthPlugin, strip_cred_scheme,
};
