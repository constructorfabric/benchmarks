//! `SeaORM` entities of the mini-chat tables. Every entity is tenant-scoped
//! through Secure ORM (`tenant_col = "tenant_id"`); `chats`, `quota_usage`
//! and `message_reactions` also declare the owner column `user_id`.

pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
