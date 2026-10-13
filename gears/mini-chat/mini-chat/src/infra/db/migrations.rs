//! Schema migrations (`cpt-cf-mini-chat-algo-gear-foundation-run-migrations`).
//!
//! * `m0001_initial`: the nine P1 tables, their constraints and indexes.
//! * the shared platform outbox migrations under [`OUTBOX_TABLE_PREFIX`].
//!
//! The runtime applies the set through the `db` capability before `init`.

use sea_orm_migration::prelude::*;

mod m0001_initial;

/// Table prefix of the shared outbox tables (`mini_chat__outbox_*`).
pub const OUTBOX_TABLE_PREFIX: &str = "mini_chat__outbox";

/// Migration set of the gear (P1 tables plus the shared outbox tables).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-no-mcp-tables
        // Only the P1 schema: no `mcp_servers`, `mcp_server_tools`,
        // `role_mcp_servers` or `tenant_closure` migration exists.
        let mut migrations: Vec<Box<dyn MigrationTrait>> = vec![Box::new(m0001_initial::Migration)];
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-no-mcp-tables
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-outbox-migrations
        match toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {
            Ok(outbox) => migrations.extend(outbox),
            Err(error) => {
                // `migrations()` cannot return an error: refuse to boot without
                // the outbox tables by adding a migration that always fails.
                tracing::error!(%error, prefix = OUTBOX_TABLE_PREFIX, "invalid outbox table prefix");
                migrations.push(Box::new(InvalidOutboxPrefix));
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-outbox-migrations
        migrations
    }
}

/// Placeholder that fails the migration run when the outbox prefix is invalid.
#[derive(DeriveMigrationName)]
struct InvalidOutboxPrefix;

#[async_trait::async_trait]
impl MigrationTrait for InvalidOutboxPrefix {
    async fn up(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(format!(
            "mini-chat outbox table prefix `{OUTBOX_TABLE_PREFIX}` is invalid"
        )))
    }
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
