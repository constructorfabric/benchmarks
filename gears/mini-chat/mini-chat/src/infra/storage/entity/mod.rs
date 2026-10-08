//! `SeaORM` entities for the mini-chat tables (DESIGN §3.7).
//!
//! Every table is tenant-scoped (`tenant_col = "tenant_id"`). Owner scoping
//! (`owner_col = "user_id"`) is declared on `chats`, `message_reactions` and
//! `quota_usage`; child tables of a chat are reached through an owner-scoped
//! chat query and then filtered by `chat_id` with a tenant-only scope.

pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
