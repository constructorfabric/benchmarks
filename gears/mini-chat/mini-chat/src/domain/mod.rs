//! Domain layer: business rules, PEP orchestration, quota and context planning.

pub mod authz;
pub mod context;
pub mod error;
pub mod quota_math;
pub mod sanitize;
pub mod service;
