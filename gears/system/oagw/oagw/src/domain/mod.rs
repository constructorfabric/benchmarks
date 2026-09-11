// Created: 2026-09-02 by Constructor Tech
//! Domain layer: the resource model, the in-memory store, the control-plane
//! service, the OData list query subset and the plugin contracts.

pub mod builtins;
pub mod model;
pub mod plugin;
pub mod query;
pub mod service;
pub mod store;

pub use service::{Caller, ControlPlane, validate_cors};
