//! Mini-chat schema migrations (D§3.7).
//!
//! * `m20261004_000001_initial` — every mini-chat table, index and value CHECK,
//!   for PostgreSQL and `SQLite`.
//! * `m20261004_000002_sqlite_sortable_timestamps` — `SQLite` triggers keeping
//!   `OData` timestamp columns in a fixed-width (sortable) text form.
//! * `m20261005_000003_sqlite_worker_timestamps` — the same triggers for the
//!   columns the orphan watchdog and upload reaper compare with a cutoff.
//!
//! The shared outbox tables come from `toolkit_db::outbox::outbox_migrations()`
//! (default `toolkit_outbox_*` prefix); the gear appends them after these.

use sea_orm_migration::prelude::*;

pub mod m20261004_000001_initial;
pub mod m20261004_000002_sqlite_sortable_timestamps;
pub mod m20261005_000003_sqlite_worker_timestamps;

/// Mini-chat migrator.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261004_000001_initial::Migration),
            Box::new(m20261004_000002_sqlite_sortable_timestamps::Migration),
            Box::new(m20261005_000003_sqlite_worker_timestamps::Migration),
        ]
    }
}
