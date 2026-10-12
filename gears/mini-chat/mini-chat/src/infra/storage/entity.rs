//! `SeaORM` entities of the gear's tables. Every entity declares its Secure ORM dimensions
//! (DESIGN section 3.7): `tenant_id` is the tenant column of all of them; `chats`, `quota_usage`
//! and `message_reactions` also declare the owner column `user_id`.

pub mod attachments;
pub mod chat_turns;
pub mod chat_vector_stores;
pub mod chats;
pub mod message_attachments;
pub mod message_reactions;
pub mod messages;
pub mod quota_usage;
pub mod thread_summaries;
