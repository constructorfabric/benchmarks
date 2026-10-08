//! Bundled static audit plugin (`static-mini-chat-audit-plugin`): logs audit
//! events.

pub mod config;
mod gear;
pub mod service;

pub use gear::StaticAuditPlugin;
