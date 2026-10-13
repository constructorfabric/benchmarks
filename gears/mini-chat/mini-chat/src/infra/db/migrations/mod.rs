//! Gear schema migrations.

use sea_orm_migration::prelude::*;

pub mod m0001_initial;

/// Migrator for the mini-chat gear tables (the outbox tables come from the
/// platform outbox migrations, see [`super::all_migrations`]).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial::Migration)]
    }
}
