//! Domain layer: business rules, PEP, quota, context assembly, services.

pub mod authz;
pub mod clock;
pub mod context;
pub mod credits;
pub mod error;
pub mod estimation;
pub mod models;
pub mod quota;
pub mod sanitize;
pub mod services;
