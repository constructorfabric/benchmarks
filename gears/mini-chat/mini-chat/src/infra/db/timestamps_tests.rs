use sea_orm::{ConnectionTrait, Database, Statement};
use time::{Duration, OffsetDateTime, UtcOffset};
use toolkit_odata::filter::{FieldKind, ODataValue};

/// 2025-10-09T08:53:30Z plus `millis`.
fn at(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_760_000_010).unwrap() + Duration::milliseconds(millis)
}

use super::{
    PgDialect, SqliteDialect, cursor_kind, cursor_value, filter_value, sqlite_text,
    sqlite_triggers_sql,
};

#[test]
fn sqlite_text_is_fixed_width_utc() {
    assert_eq!(sqlite_text(at(0)), "2025-10-09T08:53:30.000000000Z");
    assert_eq!(
        sqlite_text(at(250).to_offset(UtcOffset::from_hms(2, 0, 0).unwrap())),
        "2025-10-09T08:53:30.250000000Z"
    );
}

#[test]
fn filter_literal_is_text_on_sqlite_only() {
    let dt = chrono::DateTime::parse_from_rfc3339("2025-10-09T08:53:30.5+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let lit = ODataValue::DateTime(dt);
    match filter_value::<SqliteDialect>(&lit) {
        ODataValue::String(s) => assert_eq!(s, "2025-10-09T08:53:30.500000000Z"),
        other => panic!("expected text, got {other:?}"),
    }
    match filter_value::<PgDialect>(&lit) {
        ODataValue::DateTime(d) => assert_eq!(d, dt),
        other => panic!("expected datetime, got {other:?}"),
    }
    match filter_value::<SqliteDialect>(&ODataValue::String("x".to_owned())) {
        ODataValue::String(s) => assert_eq!(s, "x"),
        other => panic!("expected unchanged string, got {other:?}"),
    }
}

#[test]
fn cursor_codec_follows_dialect() {
    let t = at(500);
    assert_eq!(cursor_kind::<SqliteDialect>(), FieldKind::String);
    assert_eq!(
        cursor_value::<SqliteDialect>(t),
        sea_orm::Value::String(Some("2025-10-09T08:53:30.500000000Z".to_owned()))
    );
    assert_eq!(cursor_kind::<PgDialect>(), FieldKind::DateTimeUtc);
    assert_eq!(
        cursor_value::<PgDialect>(t),
        sea_orm::Value::TimeDateTimeWithTimeZone(Some(t))
    );
}

/// The triggers rewrite every RFC 3339 UTC form to the fixed-width text.
#[tokio::test]
async fn sqlite_triggers_rewrite_inserted_and_updated_values() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    db.execute_unprepared("CREATE TABLE t (id INTEGER PRIMARY KEY, ts TEXT NOT NULL)")
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO t (id, ts) VALUES (1, '2025-10-09T08:53:30Z'), (2, '2025-10-09T08:53:30.5Z')",
    )
    .await
    .unwrap();
    for sql in sqlite_triggers_sql("t", "ts") {
        db.execute_unprepared(&sql).await.unwrap();
    }
    db.execute_unprepared(
        "INSERT INTO t (id, ts) VALUES (3, '2025-10-09T08:53:30.123456789Z'), (4, '2025-10-09T08:53:31.25Z')",
    )
    .await
    .unwrap();
    db.execute_unprepared("UPDATE t SET ts = '2025-10-09T08:53:29.000001Z' WHERE id = 4")
        .await
        .unwrap();

    let rows = db
        .query_all_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT ts FROM t ORDER BY ts",
        ))
        .await
        .unwrap();
    let got: Vec<String> = rows
        .iter()
        .map(|r| r.try_get_by_index::<String>(0).unwrap())
        .collect();
    assert_eq!(
        got,
        [
            "2025-10-09T08:53:29.000001000Z",
            "2025-10-09T08:53:30.000000000Z",
            "2025-10-09T08:53:30.123456789Z",
            "2025-10-09T08:53:30.500000000Z",
        ]
    );
}

#[test]
fn comparable_is_fixed_width_text_on_sqlite_and_native_on_pg() {
    let t = at(500);
    assert_eq!(
        super::comparable(sea_orm::DbBackend::Sqlite, t),
        sea_orm::Value::String(Some("2025-10-09T08:53:30.500000000Z".to_owned()))
    );
    assert_eq!(
        super::comparable(sea_orm::DbBackend::Postgres, t),
        sea_orm::Value::TimeDateTimeWithTimeZone(Some(t))
    );
}
