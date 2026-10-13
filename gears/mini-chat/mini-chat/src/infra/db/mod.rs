//! Database layer: entities and migrations.

use sea_orm_migration::{MigrationTrait, MigratorTrait};

pub mod entities;
pub mod migrations;
pub mod repos;

/// Gear migrations followed by the platform outbox migrations (default
/// `toolkit_outbox` table prefix).
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut all = migrations::Migrator::migrations();
    all.extend(toolkit_db::outbox::outbox_migrations());
    all
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod migrations_tests;
