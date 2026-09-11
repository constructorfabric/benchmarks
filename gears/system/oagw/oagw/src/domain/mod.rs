//! Domain layer: entities, validation, alias rules and service contracts.
//!
//! This layer has no infrastructure dependencies. Infrastructure implements its
//! traits and the transport layer maps between HTTP and these types.

pub mod alias;
pub mod error;
pub mod model;
pub mod validate;
