//! `SeaORM` entities for the nine P1 tables. Each declares its Secure ORM scoping
//! (DESIGN 3.7): tenant column `tenant_id` on every table, owner column `user_id` on
//! `chats`, `message_reactions` and `quota_usage` only, no type column anywhere.

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-declare
pub mod attachment;
pub mod chat;
pub mod chat_turn;
pub mod chat_vector_store;
pub mod message;
pub mod message_attachment;
pub mod message_reaction;
pub mod quota_usage;
pub mod thread_summary;
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-declare
