//! Static audit plugin (gear `static-mini-chat-audit-plugin`): logs the
//! audit events delivered by the mini-chat audit outbox handler.

mod gear;
pub mod service;

pub use gear::StaticAuditPluginGear;
