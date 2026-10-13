//! Domain layer: authorization (PEP), services and pure helpers (enums, credit
//! arithmetic, estimation, sanitization, MIME rules).

pub mod authz;
pub mod clock;
pub mod credits;
pub mod error;
pub mod estimation;
pub mod mime;
pub mod model;
pub mod sanitize;
pub mod services;
