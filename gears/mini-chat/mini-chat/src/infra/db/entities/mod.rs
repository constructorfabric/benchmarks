//! `SeaORM` entities for the mini-chat tables (DESIGN section 3.7).
//!
//! String-valued enumerations (`role`, `state`, `status`, ...) are stored as text
//! and exposed as Rust enums in [`crate::domain::enums`].

// Column names mirror DESIGN section 3.7 even when they repeat the table name.
#![allow(clippy::struct_field_names)]

pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
