// Created: 2026-08-29 by Constructor Tech
//! OData `$filter` / `$orderby` / `$select` / `$top` / `$skip` evaluation over
//! serialized DTOs.
//!
//! The filter grammar is intentionally small (`and`-separated
//! `field op literal` comparisons) because that is all the documented OAGW
//! queries need (`alias eq 'x'`, `enabled eq true`, `upstream_id eq '<uuid>'`,
//! `type eq 'x'`).

use serde_json::Value;

use super::dto::ListQuery;
use crate::domain::error::OagwError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparator {
    Eq,
    Ne,
}

/// Apply `$filter` and `$orderby`, returning the filtered/sorted list.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the query options cannot be parsed.
pub fn apply_filter_and_order(
    mut items: Vec<Value>,
    query: &ListQuery,
) -> Result<Vec<Value>, OagwError> {
    if let Some(filter) = &query.filter {
        items = apply_filter(items, filter)?;
    }
    if let Some(orderby) = &query.orderby {
        apply_orderby(&mut items, orderby)?;
    }
    Ok(items)
}

/// Apply `$filter`, `$orderby`, `$select`, `$skip` and `$top` to `items`,
/// returning the page and the total count before paging.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the query options cannot be parsed.
pub fn apply_query_page(
    items: Vec<Value>,
    query: &ListQuery,
) -> Result<(Vec<Value>, usize), OagwError> {
    let items = apply_filter_and_order(items, query)?;
    let total = items.len();
    let page: Vec<Value> = items
        .into_iter()
        .skip(query.offset())
        .take(query.page_size())
        .collect();
    let page = if let Some(select) = &query.select {
        page.into_iter().map(|item| project(item, select)).collect()
    } else {
        page
    };
    Ok((page, total))
}

fn apply_filter(items: Vec<Value>, filter: &str) -> Result<Vec<Value>, OagwError> {
    let clauses: Vec<String> = filter
        .split(" and ")
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if clauses.is_empty() {
        return Err(invalid_filter(filter));
    }
    let parsed: Vec<(String, Comparator, String)> = clauses
        .iter()
        .map(|clause| parse_clause(clause))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(items
        .into_iter()
        .filter(|item| parsed.iter().all(|clause| matches_clause(item, clause)))
        .collect())
}

fn invalid_filter(filter: &str) -> OagwError {
    OagwError::Validation(format!("unsupported $filter expression '{filter}'"))
}

fn parse_clause(clause: &str) -> Result<(String, Comparator, String), OagwError> {
    let (field, rest) = clause
        .split_once(char::is_whitespace)
        .ok_or_else(|| invalid_filter(clause))?;
    let (op, value) = rest
        .split_once(char::is_whitespace)
        .ok_or_else(|| invalid_filter(clause))?;
    let comparator = match op.trim() {
        "eq" => Comparator::Eq,
        "ne" => Comparator::Ne,
        other => return Err(invalid_filter(&format!("operator '{other}'"))),
    };
    Ok((field.trim().to_owned(), comparator, unquote(value.trim())))
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .map_or_else(|| trimmed.to_owned(), ToOwned::to_owned)
}

fn matches_clause(item: &Value, clause: &(String, Comparator, String)) -> bool {
    let (field, comparator, expected) = clause;
    let Some(actual) = item.get(field.as_str()) else {
        return false;
    };
    let equal = match actual {
        Value::String(value) => value == expected,
        Value::Bool(value) => expected.eq_ignore_ascii_case(&value.to_string()),
        Value::Number(value) => value.to_string() == *expected,
        _ => false,
    };
    match comparator {
        Comparator::Eq => equal,
        Comparator::Ne => !equal,
    }
}

fn apply_orderby(items: &mut [Value], orderby: &str) -> Result<(), OagwError> {
    let (field, direction) = orderby
        .split_once(char::is_whitespace)
        .map_or((orderby.trim(), "asc"), |(field, dir)| {
            (field.trim(), dir.trim())
        });
    if !matches!(direction, "asc" | "desc") {
        return Err(OagwError::Validation(format!(
            "unsupported $orderby direction '{direction}'"
        )));
    }
    let key = field.to_owned();
    items.sort_by(|left, right| {
        let ordering = compare_by(left.get(&key), right.get(&key));
        if direction == "desc" {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(())
}

fn compare_by(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(Value::String(left)), Some(Value::String(right))) => left.cmp(right),
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    }
}

fn project(item: Value, select: &str) -> Value {
    let mut projected = serde_json::Map::new();
    for field in select.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        if let Some(value) = item.get(field) {
            projected.insert(field.to_owned(), value.clone());
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(alias: &str, enabled: bool) -> Value {
        serde_json::json!({ "alias": alias, "enabled": enabled })
    }

    #[test]
    fn filters_and_orders() {
        let items = vec![
            item("zeta.example.com", true),
            item("alpha.example.com", false),
            item("mike.example.com", true),
        ];
        let query = ListQuery {
            filter: Some("enabled eq true".to_owned()),
            orderby: Some("alias asc".to_owned()),
            select: None,
            top: None,
            skip: None,
        };
        let page = apply_query_page(items, &query).expect("applied");
        assert_eq!(page.0.len(), 2);
        assert_eq!(page.0[0]["alias"], "mike.example.com");
    }

    #[test]
    fn top_is_clamped_and_skip_applies() {
        let items: Vec<Value> = (0..150)
            .map(|index| serde_json::json!({ "alias": format!("h{index:03}.example.com") }))
            .collect();
        let query = ListQuery {
            filter: None,
            orderby: Some("alias asc".to_owned()),
            select: None,
            top: Some(500),
            skip: Some(99),
        };
        let page = apply_query_page(items, &query).expect("applied");
        assert_eq!(page.0.len(), 51);
        assert_eq!(page.0[0]["alias"], "h099.example.com");
    }

    #[test]
    fn select_projects_fields() {
        let items = vec![serde_json::json!({
            "id": "1", "alias": "a.example.com", "server": { "endpoints": [] }
        })];
        let query = ListQuery {
            filter: None,
            orderby: None,
            select: Some("id,alias".to_owned()),
            top: None,
            skip: None,
        };
        let page = apply_query_page(items, &query).expect("applied");
        assert!(page.0[0].get("server").is_none());
        assert_eq!(page.0[0]["alias"], "a.example.com");
    }

    #[test]
    fn rejects_unknown_operator() {
        let items = vec![item("a.example.com", true)];
        let query = ListQuery {
            filter: Some("alias contains 'a'".to_owned()),
            orderby: None,
            select: None,
            top: None,
            skip: None,
        };
        assert!(apply_query_page(items, &query).is_err());
    }
}
