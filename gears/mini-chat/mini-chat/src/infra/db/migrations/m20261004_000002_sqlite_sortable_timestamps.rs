//! `SQLite` only: keep the `OData`-filtered / ordered timestamp columns in a
//! fixed-width text form so text comparison is chronological (see
//! [`crate::infra::db::timestamps`]). No-op on PostgreSQL.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

use crate::infra::db::timestamps::{
    SORTABLE_TIMESTAMP_COLUMNS, sqlite_drop_triggers_sql, sqlite_triggers_sql,
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        let conn = manager.get_connection();
        for (table, column) in SORTABLE_TIMESTAMP_COLUMNS {
            for sql in sqlite_triggers_sql(table, column) {
                conn.execute_unprepared(&sql).await?;
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        let conn = manager.get_connection();
        for (table, column) in SORTABLE_TIMESTAMP_COLUMNS {
            for sql in sqlite_drop_triggers_sql(table, column) {
                conn.execute_unprepared(&sql).await?;
            }
        }
        Ok(())
    }
}
