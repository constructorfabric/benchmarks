//! `SeaORM` migrations of the gear: the schema of DESIGN section 3.7 plus the platform outbox
//! tables ("Gear migrations include the outbox migrations").

use sea_orm_migration::prelude::*;
use toolkit_db::outbox::outbox_migrations_with_prefix;

mod m0001_initial_schema;

/// Table prefix of the gear's outbox tables. The outbox runtime and its migrations must use the
/// same prefix.
pub const OUTBOX_TABLE_PREFIX: &str = "mini_chat__outbox";

/// The schema migrations of the gear (without the outbox tables).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m0001_initial_schema::Migration)]
    }
}

/// Every migration the gear runs: the schema followed by the outbox tables.
///
/// # Panics
/// Panics when the compile-time outbox table prefix is invalid; booting without the outbox tables
/// would make every chat deletion fail.
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut migrations = Migrator::migrations();
    match outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {
        Ok(outbox) => migrations.extend(outbox),
        Err(err) => {
            panic!("mini-chat outbox table prefix '{OUTBOX_TABLE_PREFIX}' is invalid: {err}")
        }
    }
    migrations
}
