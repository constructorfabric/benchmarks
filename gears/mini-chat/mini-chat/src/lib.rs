//! mini-chat gear: multi-tenant AI chat.
//!
//! Layout (see `docs/toolkit_unified_system/02_gear_layout_and_sdk_pattern.md`):
//! public contracts live in `cf-gears-mini-chat-sdk`; this crate holds the
//! gear registration ([`gear`]), configuration ([`config`]), the `domain`
//! services (authorization, errors, models), the REST `api` and the `infra`
//! layer (persistence, plugin gateways, bundled plugins).

// The gear depends on these gears at runtime; linking them registers their
// gears (and GTS schemas) in the link-time inventory.
use authn_resolver as _;
use authz_resolver as _;
use oagw as _;
use types_registry as _;

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
