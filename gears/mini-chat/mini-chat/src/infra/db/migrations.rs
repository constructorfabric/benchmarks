//! Schema migrations of the mini-chat gear.
//!
//! [`Migrator`] owns the domain tables (DESIGN 3.7). [`gear_migrations`] is
//! what `DatabaseCapability::migrations()` returns: the domain migrations
//! followed by the shared outbox migrations of the platform.

mod m20260301_000001_initial_schema;

use sea_orm_migration::{MigrationTrait, MigratorTrait};

/// Domain schema migrator (no outbox DDL).
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260301_000001_initial_schema::Migration)]
    }
}

/// Domain migrations plus the toolkit outbox migrations.
#[must_use]
pub fn gear_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut migrations = Migrator::migrations();
    migrations.extend(toolkit_db::outbox::outbox_migrations());
    migrations
}
