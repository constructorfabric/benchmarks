//! `SeaORM` migrations of the gear.
//!
//! `m0001_initial_schema` creates the nine P1 tables (DESIGN 3.7). The platform outbox
//! migrations are added by [`all_migrations`], which is what the gear declares.

use sea_orm_migration::prelude::*;

pub mod m0001_initial_schema;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial_schema::Migration)]
    }
}

/// The gear migration list: [`Migrator`] plus the platform outbox migrations
/// (`toolkit_db::outbox::outbox_migrations()`, default `toolkit_outbox_*` tables).
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut migrations = Migrator::migrations();
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-outbox-tables
    // the shared outbox tables (default `toolkit_outbox_*` prefix) exist before the pipeline starts
    migrations.extend(toolkit_db::outbox::outbox_migrations());
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-outbox-tables
    migrations
}
