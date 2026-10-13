//! `SeaORM` entities of the P1 schema (DESIGN 3.7), each declaring its Secure
//! ORM dimensions:
//!
//! | Entity | tenant | resource | owner |
//! |---|---|---|---|
//! | `chats` | `tenant_id` | `id` | `user_id` |
//! | `messages`, `chat_turns`, `attachments`, `thread_summaries` | `tenant_id` | `id` | none (owner isolation comes from the parent chat) |
//! | `message_attachments`, `chat_vector_stores` | `tenant_id` | none | none |
//! | `quota_usage`, `message_reactions` | `tenant_id` | `id` | `user_id` |
//!
//! Repositories (`infra::db::repo`) go through `.secure().scope_with(..)`;
//! child tables are queried with a tenant scope and a `chat_id` obtained from
//! an owner-scoped chat query.

pub mod attachments;
pub mod chat_turns;
pub mod chat_vector_stores;
pub mod chats;
pub mod message_attachments;
pub mod message_reactions;
pub mod messages;
pub mod quota_usage;
pub mod thread_summaries;
