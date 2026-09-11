//! `OData` query options over the platform's parsed query.
//!
//! The gear lists upstreams, routes and plugin bindings with the platform's
//! `OData` extractor, which parses `$filter`, `$orderby`, `$select` and `$top`
//! before the handler runs. This module evaluates the parsed filter against a
//! record, applies the ordering and projects the selected fields, so every list
//! operation in [`crate::api::rest::handlers`] shares one set of semantics.

use serde_json::Value;
use toolkit_odata::ast::{CompareOperator, Expr, Value as ODataValue};
use toolkit_odata::{ODataOrderBy, SortDir};

/// Default page size when no `$top` is supplied (see `docs/DESIGN.md`).
pub const DEFAULT_PAGE_SIZE: u64 = 50;

/// Maximum page size accepted on a list operation.
pub const MAX_PAGE_SIZE: u64 = 100;

/// Evaluate a parsed `$filter` against a JSON record.
#[must_use]
pub fn evaluate(expr: &Expr, record: &Value) -> bool {
    match expr {
        Expr::And(a, b) => evaluate(a, record) && evaluate(b, record),
        Expr::Or(a, b) => evaluate(a, record) || evaluate(b, record),
        Expr::Not(a) => !evaluate(a, record),
        Expr::Compare(left, op, right) => {
            compare(&resolve(left, record), *op, &resolve(right, record))
        }
        Expr::In(left, options) => {
            let actual = resolve(left, record);
            options
                .iter()
                .any(|candidate| values_equal(&actual, &resolve(candidate, record)))
        }
        // No documented OAGW list uses a filter function; the platform parser
        // accepts them, so they simply never match.
        Expr::Function(..) => false,
        // A bare identifier or literal is a truthiness test on the field.
        Expr::Identifier(_) | Expr::Value(_) => is_truthy(&resolve(expr, record)),
    }
}

/// Read the value an expression denotes in `record`: a field lookup for an
/// identifier, a literal for a value.
fn resolve(expr: &Expr, record: &Value) -> Value {
    match expr {
        Expr::Identifier(name) => record.get(name).cloned().unwrap_or(Value::Null),
        Expr::Value(value) => literal(value),
        _ => Value::Null,
    }
}

/// Convert a parsed `OData` literal into its JSON form.
fn literal(value: &ODataValue) -> Value {
    match value {
        ODataValue::String(s) => Value::String(s.clone()),
        ODataValue::Bool(b) => Value::Bool(*b),
        ODataValue::Number(n) => n
            .to_string()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
        ODataValue::Uuid(u) => Value::String(u.to_string()),
        ODataValue::DateTime(dt) => Value::String(dt.to_rfc3339()),
        ODataValue::Date(d) => Value::String(d.to_string()),
        ODataValue::Time(t) => Value::String(t.to_string()),
        ODataValue::Null => Value::Null,
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    compare(a, CompareOperator::Eq, b)
}

fn compare(actual: &Value, op: CompareOperator, expected: &Value) -> bool {
    let Some(ord) = ordering(actual, expected) else {
        return false;
    };
    match op {
        CompareOperator::Eq => ord == std::cmp::Ordering::Equal,
        CompareOperator::Ne => ord != std::cmp::Ordering::Equal,
        CompareOperator::Gt => ord == std::cmp::Ordering::Greater,
        CompareOperator::Ge => ord != std::cmp::Ordering::Less,
        CompareOperator::Lt => ord == std::cmp::Ordering::Less,
        CompareOperator::Le => ord != std::cmp::Ordering::Greater,
    }
}

/// Total order over two comparable JSON values, `None` when they are of
/// different kinds or incomparable.
fn ordering(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::String(a), Value::String(b)) => {
            Some(a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
        }
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .and_then(|x| b.as_f64().and_then(|y| x.partial_cmp(&y))),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

/// Order records in place by a parsed `$orderby`.
pub fn order(records: &mut [Value], order: &ODataOrderBy) {
    if order.0.is_empty() {
        return;
    }
    let keys = order.0.clone();
    records.sort_by(|a, b| {
        for key in &keys {
            let left = a.get(&key.field).unwrap_or(&Value::Null);
            let right = b.get(&key.field).unwrap_or(&Value::Null);
            let Some(cmp) = ordering(left, right) else {
                continue;
            };
            let cmp = if key.dir == SortDir::Desc {
                cmp.reverse()
            } else {
                cmp
            };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Project `$select` fields, returning the record unchanged when no select is
/// present.
#[must_use]
pub fn project(record: &Value, select: Option<&[String]>) -> Value {
    let Some(fields) = select else {
        return record.clone();
    };
    if fields.is_empty() {
        return record.clone();
    }
    let Some(obj) = record.as_object() else {
        return record.clone();
    };
    Value::Object(
        fields
            .iter()
            .filter_map(|field| obj.get(field).map(|v| (field.clone(), v.clone())))
            .collect(),
    )
}

/// Clamp a `$top` to the documented page bounds.
#[must_use]
pub fn page_size(limit: Option<u64>) -> u64 {
    limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
mod odata_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn record() -> Value {
        serde_json::json!({
            "alias": "api.partner.com",
            "name": "Partner",
            "enabled": true,
            "priority": 3
        })
    }

    fn parse_filter(raw: &str) -> Expr {
        toolkit_odata::parse_filter_string(raw)
            .expect("parsable filter")
            .into_expr()
    }

    #[test]
    fn equality_filters_match_records() {
        let expr = parse_filter("alias eq 'api.partner.com'");
        assert!(evaluate(&expr, &record()));
        let other = serde_json::json!({"alias": "other.partner.com"});
        assert!(!evaluate(&expr, &other));
    }

    #[test]
    fn string_comparison_is_case_insensitive() {
        let expr = parse_filter("alias eq 'API.PARTNER.COM'");
        assert!(evaluate(&expr, &record()), "alias lookups ignore case");
    }

    #[test]
    fn numeric_and_boolean_filters_match() {
        assert!(evaluate(&parse_filter("priority gt 2"), &record()));
        assert!(!evaluate(&parse_filter("priority ge 4"), &record()));
        assert!(evaluate(&parse_filter("enabled eq true"), &record()));
        assert!(!evaluate(&parse_filter("enabled eq false"), &record()));
    }

    #[test]
    fn conjunctions_or_and_negation_work() {
        let both = parse_filter("alias eq 'api.partner.com' and enabled eq true");
        assert!(evaluate(&both, &record()));
        let either = parse_filter("alias eq 'nope' or priority lt 10");
        assert!(evaluate(&either, &record()));
        let not = parse_filter("not (alias eq 'api.partner.com')");
        assert!(!evaluate(&not, &record()));
    }

    #[test]
    fn in_matches_any_candidate() {
        let expr = parse_filter("alias in ('a.partner.com', 'api.partner.com')");
        assert!(evaluate(&expr, &record()));
        let expr = parse_filter("alias in ('a.partner.com', 'b.partner.com')");
        assert!(!evaluate(&expr, &record()));
    }

    #[test]
    fn missing_fields_never_match() {
        let expr = parse_filter("alias eq 'api.partner.com'");
        assert!(!evaluate(&expr, &serde_json::json!({"name": "Partner"})));
        assert!(!evaluate(&parse_filter("priority gt 1"), &Value::Null));
    }

    #[test]
    fn ordering_applies_desc_and_asc() {
        let mut records = vec![
            serde_json::json!({"priority": 1}),
            serde_json::json!({"priority": 5}),
            serde_json::json!({"priority": 3}),
        ];
        let order = toolkit::api::odata::parse_orderby("priority desc").expect("parsable orderby");
        super::order(&mut records, &order);
        assert_eq!(records[0]["priority"], 5);

        let order = toolkit::api::odata::parse_orderby("priority asc").expect("parsable orderby");
        super::order(&mut records, &order);
        assert_eq!(records[0]["priority"], 1);
    }

    #[test]
    fn select_projects_only_the_named_fields() {
        let fields = ["alias".to_owned(), "name".to_owned()];
        let projected = project(&record(), Some(fields.as_slice()));
        assert_eq!(projected.as_object().unwrap().len(), 2);
        assert!(projected.get("alias").is_some());
        assert!(projected.get("priority").is_none());
        let untouched = project(&record(), None);
        assert_eq!(untouched.as_object().unwrap().len(), 4);
    }

    #[test]
    fn page_size_is_clamped_to_the_documented_bounds() {
        assert_eq!(page_size(None), DEFAULT_PAGE_SIZE);
        assert_eq!(page_size(Some(10)), 10);
        assert_eq!(page_size(Some(1_000)), MAX_PAGE_SIZE);
        assert_eq!(page_size(Some(0)), 1);
    }
}
