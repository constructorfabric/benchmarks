//! `OData` compatibility shims for the list endpoints (DESIGN "List messages").

use toolkit_odata::ODataQuery;
use toolkit_odata::ast::{Expr, Value};
use uuid::Uuid;

/// Accept quoted literals for typed fields (`id eq '<uuid>'`, as in DESIGN's
/// turn-status follow-up, and `created_at gt '<rfc3339>'`).
#[must_use]
pub fn normalize_literals(e: Expr) -> Expr {
    fn typed(field: &str, v: Value) -> Value {
        match (field, v) {
            ("id", Value::String(s)) => Uuid::parse_str(&s).map_or(Value::String(s), Value::Uuid),
            ("created_at" | "updated_at", Value::String(s)) => chrono::DateTime::parse_from_rfc3339(&s)
                .map_or(Value::String(s), |d| Value::DateTime(d.with_timezone(&chrono::Utc))),
            (_, v) => v,
        }
    }
    fn side(field: &str, e: Expr) -> Expr {
        match e {
            Expr::Value(v) => Expr::Value(typed(field, v)),
            other => other,
        }
    }
    match e {
        Expr::And(a, b) => Expr::And(Box::new(normalize_literals(*a)), Box::new(normalize_literals(*b))),
        Expr::Or(a, b) => Expr::Or(Box::new(normalize_literals(*a)), Box::new(normalize_literals(*b))),
        Expr::Not(a) => Expr::Not(Box::new(normalize_literals(*a))),
        Expr::Compare(l, op, r) => match (*l, *r) {
            (Expr::Identifier(f), r) => {
                let r = side(&f, r);
                Expr::Compare(Box::new(Expr::Identifier(f)), op, Box::new(r))
            }
            (l, Expr::Identifier(f)) => {
                let l = side(&f, l);
                Expr::Compare(Box::new(l), op, Box::new(Expr::Identifier(f)))
            }
            (l, r) => Expr::Compare(Box::new(l), op, Box::new(r)),
        },
        Expr::In(l, items) => match *l {
            Expr::Identifier(f) => {
                let items = items.into_iter().map(|i| side(&f, i)).collect();
                Expr::In(Box::new(Expr::Identifier(f)), items)
            }
            l => Expr::In(Box::new(l), items),
        },
        other => other,
    }
}

/// `SQLite` stores timestamps as text with a `Z` suffix while the `OData`
/// cursor binds `+00:00`; at the cursor boundary the stored text compares
/// greater than the bound key, so a `>` continuation would repeat the boundary
/// row. Moving the key forward by one microsecond (timestamps are stored at
/// microsecond precision) restores strict `>` semantics there.
pub fn sqlite_cursor_fix(mut q: ODataQuery, backend: sea_orm::DbBackend) -> ODataQuery {
    if backend != sea_orm::DbBackend::Sqlite {
        return q;
    }
    let Some(c) = q.cursor.as_mut() else {
        return q;
    };
    let tokens: Vec<String> = c.s.split(',').map(|t| t.trim().to_owned()).collect();
    for (i, tok) in tokens.iter().enumerate() {
        let (asc, name) = match tok.split_at_checked(1) {
            Some(("+", n)) => (true, n),
            Some(("-", n)) => (false, n),
            _ => (true, tok.as_str()),
        };
        if name != "created_at" && name != "updated_at" {
            continue;
        }
        let greater = asc == (c.d != "bwd");
        if !greater {
            continue;
        }
        if let Some(k) = c.k.get_mut(i)
            && let Ok(t) = chrono::DateTime::parse_from_rfc3339(k)
        {
            let next = t.with_timezone(&chrono::Utc) + chrono::Duration::microseconds(1);
            *k = next.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        }
    }
    q
}

#[cfg(test)]
#[path = "odata_compat_tests.rs"]
mod tests;
