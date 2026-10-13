//! Gear schema migrations.

use sea_orm_migration::MigrationTrait;

pub mod m0001_initial;

/// Gear migrations in application order (the outbox migrations are appended by the gear).
#[must_use]
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(m0001_initial::Migration)]
}
