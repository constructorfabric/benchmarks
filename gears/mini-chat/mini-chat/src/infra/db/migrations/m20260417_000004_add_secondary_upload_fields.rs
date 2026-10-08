use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        // Generic per-attachment secondary-upload slot. Currently consumed by
        // the Anthropic Files API parallel upload; the column names are
        // provider-agnostic so a future provider (Bedrock, Vertex, etc.)
        // can reuse the same slot without another migration.
        //
        // `secondary_provider_kind` discriminates which provider's id is in
        // `secondary_file_id` — NULL means upload was never attempted, mirrors
        // the `not_attempted` status. Both null together is the initial state.
        //
        // Same DDL works on Postgres and SQLite — both support the subset
        // used here (VARCHAR, CHECK, NOT NULL with DEFAULT).
        conn.execute_unprepared(
            "ALTER TABLE attachments ADD COLUMN secondary_file_id VARCHAR(128)",
        )
        .await?;
        conn.execute_unprepared(
            "ALTER TABLE attachments ADD COLUMN secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted' \
             CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed'))"
        ).await?;
        conn.execute_unprepared(
            "ALTER TABLE attachments ADD COLUMN secondary_provider_kind VARCHAR(32) \
             CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic'))",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        let backend = manager.get_database_backend();
        // SQLite < 3.35.0 has no `DROP COLUMN`; tolerate failure there. On
        // Postgres any error is a real schema problem and must propagate.
        for sql in [
            "ALTER TABLE attachments DROP COLUMN secondary_provider_kind",
            "ALTER TABLE attachments DROP COLUMN secondary_status",
            "ALTER TABLE attachments DROP COLUMN secondary_file_id",
        ] {
            match conn.execute_unprepared(sql).await {
                Ok(_) => {}
                Err(e) if backend == sea_orm::DatabaseBackend::Sqlite => {
                    tracing::warn!(error = %e, sql, "ignoring DROP COLUMN failure on SQLite");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
