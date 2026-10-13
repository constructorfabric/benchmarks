//! `SeaORM` entities of the P1 tables.
//!
//! Every entity derives `Scopable` with the scoping columns of
//! `cpt-cf-mini-chat-dod-gear-foundation-entities-scoping`; none declares a type
//! column. Child tables are reached only through
//! [`crate::infra::db::repos::chat_access`], which first loads the chat through
//! the owner-scoped query.
//!
//! | Entity | tenant | owner | resource |
//! |---|---|---|---|
//! | `chats` | `tenant_id` | `user_id` | `id` |
//! | `messages`, `chat_turns`, `attachments`, `thread_summaries` | `tenant_id` | none | `id` |
//! | `message_attachments`, `chat_vector_stores` | `tenant_id` | none | none |
//! | `quota_usage`, `message_reactions` | `tenant_id` | `user_id` | `id` |
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-entities-scoping:p1

// Every entity struct is named `Model` (`SeaORM` convention), so columns such as
// `model`, `effective_model` and `summary_model` trip this lint.
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
