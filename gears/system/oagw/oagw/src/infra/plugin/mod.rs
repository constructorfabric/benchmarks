// Created: 2026-08-29 by Constructor Tech
//! Built-in plugin implementations and the runtime plugin registry.
//!
//! All built-ins are constructed once at gear init and are immutable; the only
//! mutable state is the OAuth2 token cache, which never stores a failure.

pub mod auth;
pub mod guard;
pub mod registry;
pub mod transform;

pub use registry::PluginRegistry;
