//! Gear migrations (mini-chat schema + shared outbox schema).

use sea_orm_migration::prelude::*;

mod m0001_initial_schema;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial_schema::Migration)]
    }
}

/// All migrations the gear provides: its own schema followed by the shared
/// outbox tables.
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut v = Migrator::migrations();
    v.extend(toolkit_db::outbox::outbox_migrations());
    v
}
