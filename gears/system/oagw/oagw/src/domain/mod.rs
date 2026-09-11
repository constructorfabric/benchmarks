//! Domain layer: business rules, service traits and models. No dependency on
//! the transport or infrastructure layers.

pub mod alias;
pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod services;
pub mod tenant;
