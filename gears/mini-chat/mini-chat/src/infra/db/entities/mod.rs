//! `SeaORM` entities for the `mini-chat` tables (DESIGN §3.7).
//!
//! String-valued enum columns (`state`, `status`, `role`, ...) stay `String`
//! here; the domain layer converts them to typed enums.

pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
