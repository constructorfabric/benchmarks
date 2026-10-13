//! `SeaORM` entities for the mini-chat tables (D§3.7). Rust field names equal
//! the column names. UUIDs are `Uuid` (16-byte BLOB on `SQLite`, `UUID` on
//! PostgreSQL); timestamps are `time::OffsetDateTime` (UTC); the quota period
//! start is a `time::Date`.
//!
//! Secure ORM: every table is tenant-scoped (`tenant_id`); `chats`,
//! `quota_usage` and `message_reactions` are also owner-scoped (`user_id`).
//! Child tables of a chat have no owner column; repositories narrow the
//! caller's scope with `AccessScope::tenant_only()` for them, after the
//! parent chat was loaded with the full (tenant + owner) scope.

pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
