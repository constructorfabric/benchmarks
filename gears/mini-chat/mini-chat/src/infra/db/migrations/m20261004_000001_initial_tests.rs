use sea_orm_migration::sea_orm::DatabaseBackend;

use super::{INDEXES, TABLES, render};

#[test]
fn postgres_rendering_uses_native_types_and_leaves_no_placeholders() {
    for t in TABLES.iter().chain(INDEXES) {
        let sql = render(t, DatabaseBackend::Postgres).unwrap();
        assert!(!sql.contains('{'), "unrendered placeholder in: {sql}");
    }
    let chats = render(TABLES[0], DatabaseBackend::Postgres).unwrap();
    assert!(chats.contains("id UUID PRIMARY KEY"));
    assert!(chats.contains("created_at TIMESTAMPTZ NOT NULL"));
    let messages = render(TABLES[1], DatabaseBackend::Postgres).unwrap();
    assert!(messages.contains("features_used JSONB NOT NULL DEFAULT '[]'::jsonb"));
    let quota = TABLES.iter().find(|t| t.contains("quota_usage")).unwrap();
    assert!(
        render(quota, DatabaseBackend::Postgres)
            .unwrap()
            .contains("period_start DATE NOT NULL")
    );
}

#[test]
fn sqlite_rendering_declares_uuid_columns_text() {
    let chats = render(TABLES[0], DatabaseBackend::Sqlite).unwrap();
    assert!(chats.contains("id TEXT PRIMARY KEY"));
    assert!(chats.contains("is_temporary BOOLEAN NOT NULL DEFAULT 0"));
    for t in TABLES.iter().chain(INDEXES) {
        let sql = render(t, DatabaseBackend::Sqlite).unwrap();
        assert!(!sql.contains('{'), "unrendered placeholder in: {sql}");
        assert!(
            !sql.contains("UUID"),
            "PG type leaked into SQLite DDL: {sql}"
        );
    }
}

#[test]
fn mysql_is_rejected() {
    assert!(render(TABLES[0], DatabaseBackend::MySql).is_err());
}
