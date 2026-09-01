//! Domain layer — business rules with no infrastructure dependencies.
//!
//! - `dto` — shared configuration/resource models (upstreams, routes,
//!   plugins, rate limits, CORS)
//! - `error` — the domain error type and its RFC 9457 mapping
//! - `gts_helpers` — GTS identifier constants (single source of truth)
//! - `plugin` — the plugin traits (`AuthPlugin`, `GuardPlugin`,
//!   `TransformPlugin`) and their execution context types
//! - `repo` — repository traits for the control plane
//! - `services` — `ControlPlaneService` (management) and `DataPlaneService`
//!   (proxy orchestration)

pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod plugin;
pub mod repo;
pub mod services;
