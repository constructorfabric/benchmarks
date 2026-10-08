//! `SeaORM` migrations for the `mini-chat` gear.
//!
//! * `m0001_initial` — all nine tables of DESIGN §3.7 (`chats`, `messages`,
//!   `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`,
//!   `chat_vector_stores`, `quota_usage`, `message_reactions`) with their
//!   constraints and indexes, in a `PostgreSQL` and a `SQLite` variant.
//!
//! The shared outbox tables are created by the platform outbox migrations
//! (see [`super::all_migrations`]), not here.

use sea_orm_migration::prelude::*;

pub mod m0001_initial;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial::Migration)]
    }
}
