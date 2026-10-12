//! Bundled plugins (DESIGN Appendix B.1 "Bundled plugins").
//!
//! Each plugin is a separate gear with its own config section, registered by
//! the server through these module paths:
//!
//! - [`static_model_policy`] — `static-mini-chat-model-policy-plugin`;
//! - [`static_audit`] — `static-mini-chat-audit-plugin`.

pub mod static_audit;
pub mod static_model_policy;
