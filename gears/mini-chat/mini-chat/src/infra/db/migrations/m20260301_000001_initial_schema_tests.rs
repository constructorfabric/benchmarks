//! Dialect checks of the initial schema: both engines get the same table and
//! index set, differing only in column types and literals.

use super::{POSTGRES, SQLITE, TABLES, statements};

fn created_tables(sql: &[String]) -> Vec<String> {
    sql.iter()
        .filter_map(|s| s.strip_prefix("CREATE TABLE "))
        .filter_map(|s| s.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn both_dialects_create_every_p1_table_in_dependency_order() {
    for dialect in [&POSTGRES, &SQLITE] {
        let sql = statements(dialect);
        assert_eq!(created_tables(&sql), TABLES);
    }
}

#[test]
fn both_dialects_emit_the_same_number_of_statements() {
    assert_eq!(statements(&POSTGRES).len(), statements(&SQLITE).len());
}

#[test]
fn postgres_uses_native_types() {
    let sql = statements(&POSTGRES).join("\n");
    assert!(sql.contains("UUID PRIMARY KEY"));
    assert!(sql.contains("TIMESTAMPTZ"));
    assert!(sql.contains("JSONB"));
    assert!(sql.contains("BYTEA"));
    assert!(!sql.contains("BLOB"));
    assert!(!sql.contains("CURRENT_TIMESTAMP"));
}

#[test]
fn sqlite_declares_uuids_as_text_and_has_no_postgres_only_types() {
    let sql = statements(&SQLITE).join("\n");
    assert!(sql.contains("id TEXT PRIMARY KEY"));
    assert!(sql.contains("BLOB"));
    for postgres_only in ["UUID", "TIMESTAMPTZ", "JSONB", "BYTEA", "::jsonb", "now()"] {
        assert!(
            !sql.contains(postgres_only),
            "`{postgres_only}` leaked into the SQLite variant"
        );
    }
}

#[test]
fn the_stale_upload_index_is_plain_in_both_dialects() {
    for dialect in [&POSTGRES, &SQLITE] {
        let index = statements(dialect)
            .into_iter()
            .find(|s| s.contains("idx_attachments_stale_upload"))
            .expect("index statement");
        assert!(!index.contains("WHERE"), "{index}");
    }
}
