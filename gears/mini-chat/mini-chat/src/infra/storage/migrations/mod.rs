//! `SeaORM` migrations of the mini-chat gear. The shared outbox tables are
//! created by the toolkit outbox migrations, appended by
//! [`all_migrations`].

use sea_orm_migration::prelude::*;

pub mod m0001_initial_schema;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial_schema::Migration)]
    }
}

/// Gear migrations followed by the shared outbox migrations.
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut out = Migrator::migrations();
    out.extend(toolkit_db::outbox::outbox_migrations());
    out
}
