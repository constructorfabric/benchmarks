//! Infrastructure layer — concrete implementations behind the domain traits.
//!
//! - `plugin` — built-in plugins and the per-family registries
//! - `proxy` — HTTP forwarding, WebSocket bridging, rate limiting, CORS
//! - `storage` — in-memory repository implementations
//! - `type_provisioning` — link-time GTS type-schema / instance catalog

pub mod plugin;
pub mod proxy;
pub mod storage;
pub mod type_provisioning;
