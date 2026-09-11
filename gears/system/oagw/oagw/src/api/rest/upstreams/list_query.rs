//! `cpt-cf-oagw-dod-list-query-params`: `$filter`/`$select`/`$orderby`/
//! `$top`/`$skip` support for `GET /oagw/v1/upstreams`.
//!
//! `$top`/`$skip` parsing itself is delegated to the shared
//! `super::super::page_params` module so this endpoint and
//! `GET /oagw/v1/routes` cannot re-diverge on malformed input (RF-005); see
//! that module's doc comment.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::api::rest::page_params::{self, PageParams};

/// Raw `OData`-style query parameters bound off the URL.
#[derive(Debug, Deserialize, Default)]
pub(super) struct ListQueryParams {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<String>,
    #[serde(rename = "$skip")]
    pub skip: Option<String>,
}

/// `$top`/`$skip`, defaulted/capped/rejected per the shared contract
/// (`cpt-cf-oagw-dod-list-query-params`).
pub(super) fn parse_page(top: Option<&str>, skip: Option<&str>) -> Result<PageParams, String> {
    page_params::parse_page_params(top, skip)
}

enum FilterLiteral {
    Str(String),
    Bool(bool),
    Num(f64),
}

fn parse_literal(raw: &str) -> FilterLiteral {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        FilterLiteral::Str(inner.to_owned())
    } else if raw == "true" {
        FilterLiteral::Bool(true)
    } else if raw == "false" {
        FilterLiteral::Bool(false)
    } else if let Ok(n) = raw.parse::<f64>() {
        FilterLiteral::Num(n)
    } else {
        FilterLiteral::Str(raw.to_owned())
    }
}

fn parse_eq_clause(clause: &str) -> Result<(String, FilterLiteral), String> {
    let mut parts = clause.splitn(3, ' ');
    let (Some(field), Some(op), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("unsupported $filter clause `{clause}`"));
    };
    if op != "eq" {
        return Err(format!(
            "unsupported $filter operator `{op}`; only `eq` is supported"
        ));
    }
    Ok((field.to_owned(), parse_literal(rest)))
}

fn field_eq(item: &Value, field: &str, literal: &FilterLiteral) -> bool {
    let Some(value) = item.get(field) else {
        return false;
    };
    match literal {
        FilterLiteral::Str(s) => value.as_str() == Some(s.as_str()),
        FilterLiteral::Bool(b) => value.as_bool() == Some(*b),
        FilterLiteral::Num(n) => value.as_f64() == Some(*n),
    }
}

/// Minimal `$filter` support: `and`-joined `field eq literal` clauses.
pub(super) fn apply_filter(items: Vec<Value>, filter: &str) -> Result<Vec<Value>, String> {
    let mut predicates = Vec::new();
    for clause in filter.split(" and ").map(str::trim) {
        predicates.push(parse_eq_clause(clause)?);
    }
    Ok(items
        .into_iter()
        .filter(|item| {
            predicates
                .iter()
                .all(|(field, lit)| field_eq(item, field, lit))
        })
        .collect())
}

fn compare_json(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Number(x)), Some(Value::Number(y))) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

/// `$orderby`: comma-separated `field [asc|desc]` clauses, `asc` default.
pub(super) fn apply_orderby(mut items: Vec<Value>, orderby: &str) -> Vec<Value> {
    let clauses: Vec<(&str, bool)> = orderby
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            let mut parts = c.split_whitespace();
            let field = parts.next().unwrap_or("");
            let desc = matches!(parts.next(), Some("desc"));
            (field, desc)
        })
        .collect();

    items.sort_by(|a, b| {
        for (field, desc) in &clauses {
            let ord = compare_json(a.get(*field), b.get(*field));
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    items
}

/// `$select`: project each item down to the requested field set.
pub(super) fn apply_select(items: Vec<Value>, select: &str) -> Vec<Value> {
    let fields: Vec<&str> = select
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    items
        .into_iter()
        .map(|item| {
            let mut out = Map::new();
            if let Value::Object(map) = &item {
                for field in &fields {
                    if let Some(v) = map.get(*field) {
                        out.insert((*field).to_owned(), v.clone());
                    }
                }
            }
            Value::Object(out)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn top_defaults_to_50_and_caps_at_100() {
        assert_eq!(parse_page(None, None).unwrap().top, 50);
        assert_eq!(parse_page(Some("30"), None).unwrap().top, 30);
        assert_eq!(parse_page(Some("500"), None).unwrap().top, 100);
    }

    #[test]
    fn skip_defaults_to_zero() {
        assert_eq!(parse_page(None, None).unwrap().skip, 0);
        assert_eq!(parse_page(None, Some("5")).unwrap().skip, 5);
    }

    #[test]
    fn malformed_top_is_rejected() {
        assert!(parse_page(Some("notanumber"), None).is_err());
    }

    #[test]
    fn filter_matches_a_simple_eq_clause() {
        let items = vec![json!({"alias": "a"}), json!({"alias": "b"})];
        let filtered = apply_filter(items, "alias eq 'a'").unwrap();
        assert_eq!(filtered, vec![json!({"alias": "a"})]);
    }

    #[test]
    fn orderby_sorts_ascending_by_default() {
        let items = vec![json!({"alias": "b"}), json!({"alias": "a"})];
        let sorted = apply_orderby(items, "alias");
        assert_eq!(sorted, vec![json!({"alias": "a"}), json!({"alias": "b"})]);
    }

    #[test]
    fn select_projects_only_requested_fields() {
        let items = vec![json!({"alias": "a", "enabled": true})];
        let projected = apply_select(items, "alias");
        assert_eq!(projected, vec![json!({"alias": "a"})]);
    }
}
