use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "ALTER TABLE chat_turns ADD COLUMN web_search_completed_count INT NOT NULL DEFAULT 0",
        )
        .await?;
        conn.execute_unprepared(
            "ALTER TABLE chat_turns ADD COLUMN code_interpreter_completed_count INT NOT NULL DEFAULT 0",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared("ALTER TABLE chat_turns DROP COLUMN web_search_completed_count")
            .await?;
        conn.execute_unprepared(
            "ALTER TABLE chat_turns DROP COLUMN code_interpreter_completed_count",
        )
        .await?;
        Ok(())
    }
}
