//! Domain layer: business rules, orchestration, PEP, context assembly,
//! streaming, quota enforcement.

pub mod attachment_service;
pub mod authz;
pub mod chat_service;
pub mod context;
pub mod credits;
pub mod finalize;
pub mod model_service;
pub mod quota_service;
pub mod quota_warnings;
pub mod services;
pub mod stream_service;
pub mod turn_service;
