// Created: 2026-08-31 by Constructor Tech
//! Domain layer: records, alias rules, validation, storage, the control-plane
//! service and the proxy data plane (DESIGN-LIGHT layering, see the crate
//! docs).

pub mod alias;
pub mod credentials;
pub mod lifecycle;
pub mod model;
pub mod plugin;
pub mod proxy;
pub mod service;
pub mod spec;
pub mod store;
pub mod time;
pub mod validation;

pub use service::OagwService;
