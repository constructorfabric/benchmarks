//! Gear migrations.

use sea_orm_migration::prelude::*;

pub mod m0001_initial_schema;

/// Gear migrator (outbox migrations are appended by the gear).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial_schema::Migration)]
    }
}

/// All migrations of the gear, including the shared outbox tables.
///
/// # Panics
/// Never in practice: the default outbox prefix is valid.
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut migrations = Migrator::migrations();
    migrations.extend(toolkit_db::outbox::outbox_migrations());
    migrations
}
