//! Domain layer: business rules, authorization (PEP) and repository ports.

pub mod attachment;
pub mod authz;
pub mod billing;
pub mod chat;
pub mod cleanup;
pub mod clock;
pub mod context;
pub mod credits;
pub mod error;
pub mod estimation;
pub mod llm;
pub mod message;
pub mod metrics;
pub mod models;
pub mod mutation;
pub mod orphan;
pub mod ports;
pub mod quota;
pub mod sanitize;
pub mod stream;
pub mod summary;
pub mod turn;
