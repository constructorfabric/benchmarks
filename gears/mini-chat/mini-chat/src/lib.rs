//! `mini-chat` gear: multi-tenant AI chat with SSE streaming, attachments and quotas.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;

// Force-link the gears mini-chat depends on (the example server's `mini-chat` feature
// does not enable them separately).
use authn_resolver as _;
use authz_resolver as _;
use oagw as _;
use types_registry as _;
