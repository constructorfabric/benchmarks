//! Domain layer: business rules, PEP, quota, context assembly, streaming.

pub mod attachments;
pub mod catalog;
pub mod chats;
pub mod context;
pub mod credits;
pub mod errors;
pub mod events;
pub mod finalize;
pub mod messages;
pub mod odata_fields;
pub mod quota;
pub mod sanitize;
pub mod state;
pub mod stream;
pub mod summary;
pub mod turns;
pub mod watchdog;
