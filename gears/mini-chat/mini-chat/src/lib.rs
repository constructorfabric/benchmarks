//! `mini-chat` gear: multi-tenant AI chat with streaming responses, file
//! attachments, quota enforcement and turn mutations.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
pub mod service;

pub use gear::MiniChatGear;

// Gears this crate depends on at runtime (linked for inventory registration).
use authn_resolver as _;
use authz_resolver as _;
use oagw as _;
use types_registry as _;
