//! Domain layer for the OAGW gear.
//!
//! Contains the entity model ([`models`]), the control-plane error hierarchy
//! ([`error`]), the plugin abstraction ([`plugin`]), repository traits
//! ([`repo`]), and the business services ([`service`], [`alias`]).

pub mod alias;
pub mod error;
pub mod models;
pub mod plugin;
pub mod repo;
pub mod service;
