//! Gear migrations (the shared outbox migrations are appended in `gear.rs`).

use sea_orm_migration::prelude::*;

pub mod m0001_initial;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial::Migration)]
    }
}
