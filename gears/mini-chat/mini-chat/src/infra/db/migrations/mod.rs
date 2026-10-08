//! Database migrations of the mini-chat gear (PostgreSQL and SQLite).
//!
//! Every statement has a `PostgreSQL` and a `SQLite` variant where the dialects
//! differ. UUID columns are `TEXT` in `SQLite` but hold 16-byte BLOB values
//! (`SeaORM` binds `Uuid` as bytes). The shared outbox tables come from the
//! platform outbox migrations (see `gear.rs`).

use sea_orm_migration::prelude::*;

mod m20260901_000001_initial;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260901_000001_initial::Migration)]
    }
}
