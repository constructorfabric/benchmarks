//! Persistence layer: entities, migrations, repositories.

pub mod entity;
pub mod migrations;
pub mod repo;

use sea_orm_migration::MigratorTrait;

/// The gear migrator.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![Box::new(migrations::Migration)]
    }
}
