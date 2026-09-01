// Created: 2026-08-31 by Constructor Tech
//! The plugin system of the data plane (ADR-0002, ADR-0008, ADR-0009).
//!
//! * [`traits`] — the three contracts of ADR-0002 and the context they run in.
//! * [`registry`] — one registry per contract, built over the built-ins.
//! * [`secrets`] — `cred://` resolution shared by the auth plugins.
//! * one module per built-in plugin:
//!   [`noop_auth`], [`apikey_auth`], [`oauth2_client_cred_auth`],
//!   [`required_headers_guard`], [`request_id_transform`].
//!
//! # Built-in plugins
//!
//! | Family | Name | Resolvable |
//! |---|---|---|
//! | auth | `noop` | yes |
//! | auth | `apikey` | yes (needs a credential store) |
//! | auth | `oauth2_client_cred` | yes (needs a credential store) |
//! | auth | `oauth2_client_cred_basic` | yes (needs a credential store) |
//! | guard | `required_headers` | yes |
//! | transform | `request_id` | yes |
//!
//! `basic`, `bearer`, `timeout`, `cors`, `logging` and `metrics` stay
//! catalog-only (ADR-0002 "Built-in Plugins"): they are not in a registry, so a
//! binding of them is a 503 at request time and a 400 on the write path.
//!
//! # Security boundary (PRD `cpt-cf-oagw-fr-auth-injection`)
//!
//! Secret material exists only inside [`SecretString`](toolkit_auth::oauth2::SecretString)
//! from the moment the credential store returns it. It is never `Debug`-
//! formatted, never logged, never serialised into a problem document, and never
//! persisted; the token cache is keyed by tenant and subject and every hit is
//! verified against its key.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod secrets;
pub mod traits;

pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
pub use traits::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginConfig, Rejection, RequestContext,
    ResponseContext, TransformPlugin, UpstreamRef,
};
