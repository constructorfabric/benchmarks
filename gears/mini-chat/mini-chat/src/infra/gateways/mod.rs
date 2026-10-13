//! Gateways to the mini-chat plugins (model policy, audit).
//!
//! The gear talks to its plugins only through [`policy::PolicyGateway`] and
//! [`audit::AuditGateway`]. Production uses the `Plugin*` implementations, which resolve the
//! plugin lazily through types-registry and the `ClientHub` (by the configured `vendor`); tests use
//! the `Direct*` implementations over an in-process plugin client.

pub mod audit;
pub mod policy;
