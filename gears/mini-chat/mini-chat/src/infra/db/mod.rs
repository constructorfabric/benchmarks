//! Persistence of the gear's tables (DESIGN section 3.7 "Database Schemas &
//! Tables"): `SeaORM` entities, migrations and secure-ORM repositories.
//!
//! - [`entities`] — one Secure ORM scoped entity module per table;
//! - [`migrations`] — the PostgreSQL / `SQLite` schema [`Migrator`] and
//!   [`migrations()`], which also carries the shared outbox migrations;
//! - [`error`] — `DbError` / `ScopeError` -> `DomainError` mapping and
//!   unique-violation target detection;
//! - [`repo`] — Secure ORM repositories of the domain persistence ports.

pub mod entities;
pub mod error;
pub mod migrations;
pub mod repo;

pub use entities::{
    attachments, chat_turns, chat_vector_stores, chats, message_attachments, message_reactions,
    messages, quota_usage, thread_summaries,
};
pub use error::{DbTable, UniqueTarget, unique_violation_target};
pub use migrations::{Migrator, migrations};

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
