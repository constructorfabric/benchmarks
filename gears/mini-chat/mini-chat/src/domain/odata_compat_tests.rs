#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataQuery, SortDir};

use super::{normalize_literals, sqlite_cursor_fix};

fn cmp(field: &str, v: Value) -> Expr {
    Expr::Compare(Box::new(Expr::Identifier(field.into())), CompareOperator::Eq, Box::new(Expr::Value(v)))
}

#[test]
fn quoted_uuid_and_datetime_become_typed() {
    let id = uuid::Uuid::new_v4();
    let e = normalize_literals(cmp("id", Value::String(id.to_string())));
    let Expr::Compare(_, _, r) = e else { panic!() };
    assert!(matches!(*r, Expr::Value(Value::Uuid(u)) if u == id));
    let e = normalize_literals(cmp("created_at", Value::String("2026-01-02T03:04:05Z".into())));
    let Expr::Compare(_, _, r) = e else { panic!() };
    assert!(matches!(*r, Expr::Value(Value::DateTime(_))));
    let e = normalize_literals(cmp("role", Value::String("user".into())));
    let Expr::Compare(_, _, r) = e else { panic!() };
    assert!(matches!(*r, Expr::Value(Value::String(_))));
}

fn q(s: &str, d: &str) -> ODataQuery {
    ODataQuery {
        cursor: Some(CursorV1 {
            k: vec!["2026-10-03T11:03:24.628296Z".into(), "x".into()],
            o: SortDir::Asc,
            s: s.into(),
            f: None,
            d: d.into(),
        }),
        ..ODataQuery::default()
    }
}

#[test]
fn sqlite_cursor_moves_only_strict_greater_keys() {
    let fixed = sqlite_cursor_fix(q("+created_at,+id", "fwd"), sea_orm::DbBackend::Sqlite);
    assert_eq!(fixed.cursor.unwrap().k[0], "2026-10-03T11:03:24.628297000Z");
    let same = sqlite_cursor_fix(q("-updated_at,-id", "fwd"), sea_orm::DbBackend::Sqlite);
    assert_eq!(same.cursor.unwrap().k[0], "2026-10-03T11:03:24.628296Z");
    let bwd = sqlite_cursor_fix(q("-updated_at,-id", "bwd"), sea_orm::DbBackend::Sqlite);
    assert_eq!(bwd.cursor.unwrap().k[0], "2026-10-03T11:03:24.628297000Z");
    let pg = sqlite_cursor_fix(q("+created_at,+id", "fwd"), sea_orm::DbBackend::Postgres);
    assert_eq!(pg.cursor.unwrap().k[0], "2026-10-03T11:03:24.628296Z");
}
