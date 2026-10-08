use chrono::{TimeZone, Utc};
use toolkit_odata::ast::{CompareOperator as Op, Expr, Value};

use super::*;

fn dt() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap()
}

fn cmp(f: &str, op: Op, v: chrono::DateTime<Utc>) -> Expr {
    Expr::Compare(Box::new(Expr::Identifier(f.into())), op, Box::new(Expr::Value(Value::DateTime(v))))
}

fn render(e: &Expr) -> String {
    match e {
        Expr::And(a, b) => format!("({} and {})", render(a), render(b)),
        Expr::Or(a, b) => format!("({} or {})", render(a), render(b)),
        Expr::Not(a) => format!("not {}", render(a)),
        Expr::Compare(l, op, r) => format!("{} {op:?} {}", render(l), render(r)),
        Expr::Identifier(i) => i.clone(),
        Expr::Value(Value::DateTime(d)) => d.timestamp_subsec_nanos().to_string(),
        Expr::Value(_) => "v".into(),
        Expr::In(l, vs) => format!("{} in {}", render(l), vs.len()),
        Expr::Function(n, _) => n.clone(),
    }
}

#[test]
fn equality_and_strict_bounds_become_half_open_ranges() {
    let f = ["updated_at"];
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Eq, dt()), &f)), "(updated_at Ge 123456789 and updated_at Lt 123456790)");
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Ne, dt()), &f)), "(updated_at Lt 123456789 or updated_at Ge 123456790)");
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Gt, dt()), &f)), "updated_at Ge 123456790");
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Le, dt()), &f)), "updated_at Lt 123456790");
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Ge, dt()), &f)), "updated_at Ge 123456789");
    assert_eq!(render(&rewrite_timestamp_filter(cmp("updated_at", Op::Lt, dt()), &f)), "updated_at Lt 123456789");
}

#[test]
fn other_fields_and_nested_expressions() {
    let f = ["created_at"];
    let e = Expr::And(Box::new(cmp("created_at", Op::Eq, dt())), Box::new(Expr::Not(Box::new(cmp("other", Op::Eq, dt())))));
    assert_eq!(render(&rewrite_timestamp_filter(e, &f)), "((created_at Ge 123456789 and created_at Lt 123456790) and not other Eq 123456789)");
    // Reversed operands are flipped.
    let rev = Expr::Compare(Box::new(Expr::Value(Value::DateTime(dt()))), Op::Lt, Box::new(Expr::Identifier("created_at".into())));
    assert_eq!(render(&rewrite_timestamp_filter(rev, &f)), "created_at Ge 123456790");
    // `in` becomes an OR of equality ranges.
    let inl = Expr::In(Box::new(Expr::Identifier("created_at".into())), vec![Expr::Value(Value::DateTime(dt()))]);
    assert_eq!(render(&rewrite_timestamp_filter(inl, &f)), "(created_at Ge 123456789 and created_at Lt 123456790)");
}
