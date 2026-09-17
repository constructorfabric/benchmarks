// Created: 2026-09-03 by Constructor Tech
//! OData list query support for the collection endpoints.
//!
//! Implements `$filter`, `$select`, `$orderby`, `$top` and `$skip` for the
//! single-property comparisons the list surfaces need, with the page size
//! bounds taken from the gear configuration.

use serde::Serialize;
use serde_json::Value;

use crate::error::{ErrorKind, OagwError};

/// Parsed OData list query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListParams {
    /// Page size.
    pub top: Option<usize>,
    /// Number of items to skip.
    pub skip: Option<usize>,
    /// `$filter` expression.
    pub filter: Option<String>,
    /// `$orderby` clause.
    pub orderby: Option<String>,
    /// `$select` projection.
    pub select: Option<Vec<String>>,
}

impl ListParams {
    /// Parses a raw query string.
    ///
    /// # Errors
    /// Returns 400 `ValidationError` for a non-numeric `$top`/`$skip`.
    pub fn parse(query: Option<&str>) -> Result<Self, OagwError> {
        let mut params = Self::default();
        let Some(query) = query else {
            return Ok(params);
        };
        for (name, value) in form_urlencoded::parse(query.as_bytes()) {
            match name.as_ref() {
                "$top" => {
                    params.top = Some(parse_usize(&value, "$top")?);
                }
                "$skip" => {
                    params.skip = Some(parse_usize(&value, "$skip")?);
                }
                "$filter" => params.filter = Some(value.to_string()),
                "$orderby" => params.orderby = Some(value.to_string()),
                "$select" => {
                    params.select = Some(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        Ok(params)
    }
}

fn parse_usize(value: &str, name: &str) -> Result<usize, OagwError> {
    value.parse::<usize>().map_err(|_| {
        OagwError::new(
            ErrorKind::Validation,
            format!("{name} must be a non-negative integer"),
        )
    })
}

/// The envelope of a collection response.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ListEnvelope {
    /// Page of results, projected by `$select` when requested.
    pub items: Vec<Value>,
    /// Number of items in this page.
    pub count: usize,
    /// Effective page size.
    pub top: usize,
    /// Effective skip.
    pub skip: usize,
}

/// Applies the list parameters to `items` and renders the envelope.
///
/// # Errors
/// Returns 400 `ValidationError` when `$top` exceeds the configured maximum.
pub fn render<T: Serialize>(
    items: Vec<T>,
    params: &ListParams,
    top_default: usize,
    top_max: usize,
) -> Result<ListEnvelope, OagwError> {
    let top = params.top.unwrap_or(top_default);
    if top > top_max {
        return Err(OagwError::new(
            ErrorKind::Validation,
            format!("$top must not exceed {top_max}"),
        ));
    }
    let skip = params.skip.unwrap_or(0);
    let mut values: Vec<Value> = items
        .into_iter()
        .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
        .collect();
    if let Some(filter) = &params.filter {
        values.retain(|value| matches_filter(value, filter));
    }
    if let Some(orderby) = &params.orderby {
        sort_by(&mut values, orderby);
    }
    let page: Vec<Value> = values.into_iter().skip(skip).take(top).collect();
    let count = page.len();
    Ok(ListEnvelope {
        items: apply_select(page, params.select.as_ref()),
        count,
        top,
        skip,
    })
}

/// Evaluates a `field op value` predicate with `and` conjunctions.
fn matches_filter(value: &Value, filter: &str) -> bool {
    filter
        .split(" and ")
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .all(|clause| matches_clause(value, clause))
}

fn matches_clause(value: &Value, clause: &str) -> bool {
    let Some((field, operator, expected)) = parse_clause(clause) else {
        return true;
    };
    let actual = value.get(field).unwrap_or(&Value::Null);
    let rendered = match actual {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    match operator.as_str() {
        "eq" => rendered.eq_ignore_ascii_case(&expected),
        "ne" => !rendered.eq_ignore_ascii_case(&expected),
        _ => true,
    }
}

fn parse_clause(clause: &str) -> Option<(String, String, String)> {
    for operator in ["eq", "ne"] {
        let needle = format!(" {operator} ");
        if let Some(position) = clause.to_ascii_lowercase().find(&needle) {
            let field = clause[..position].trim().to_owned();
            let expected = unquote(clause[position + needle.len()..].trim());
            return Some((field, operator.to_owned(), expected));
        }
    }
    None
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .to_owned()
}

/// Sorts by the first `$orderby` field, honouring a trailing `desc`.
fn sort_by(values: &mut [Value], orderby: &str) {
    let mut parts = orderby.split_whitespace();
    let Some(field) = parts.next() else {
        return;
    };
    let descending = parts.next().is_some_and(|direction| direction.eq_ignore_ascii_case("desc"));
    values.sort_by(|a, b| {
        let left = a.get(field).unwrap_or(&Value::Null);
        let right = b.get(field).unwrap_or(&Value::Null);
        let ordering = compare_values(left, right);
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
}

fn compare_values(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .unwrap_or_default()
            .partial_cmp(&right.as_f64().unwrap_or_default())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    }
}

/// Projects the selected members of each item.
fn apply_select(items: Vec<Value>, select: Option<&Vec<String>>) -> Vec<Value> {
    let Some(fields) = select else {
        return items;
    };
    if fields.is_empty() {
        return items;
    }
    items
        .into_iter()
        .map(|item| match &item {
            Value::Object(entries) => {
                let mut projected = serde_json::Map::new();
                for field in fields {
                    if let Some(value) = entries.get(field) {
                        projected.insert(field.clone(), value.clone());
                    }
                }
                Value::Object(projected)
            }
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_list_params() {
        let params = ListParams::parse(Some("$top=5&$skip=2&$orderby=name desc")).expect("params");
        assert_eq!(params.top, Some(5));
        assert_eq!(params.skip, Some(2));
        assert_eq!(params.orderby.as_deref(), Some("name desc"));
        assert!(ListParams::parse(Some("$top=abc")).is_err());
        assert_eq!(ListParams::parse(None).ok(), Some(ListParams::default()));
    }

    #[test]
    fn applies_filter_orderby_and_paging() {
        let items = vec![
            serde_json::json!({ "name": "b", "n": 2 }),
            serde_json::json!({ "name": "a", "n": 1 }),
            serde_json::json!({ "name": "c", "n": 3 }),
        ];
        let params = ListParams {
            top: Some(2),
            skip: Some(1),
            filter: None,
            orderby: Some("name desc".to_owned()),
            select: None,
        };
        let page = render(items, &params, 50, 100).expect("render");
        assert_eq!(page.count, 2);
        assert_eq!(page.items[0]["name"], "b");
        assert_eq!(page.items[1]["name"], "a");
        assert_eq!(page.top, 2);
        assert_eq!(page.skip, 1);
    }

    #[test]
    fn top_above_the_maximum_is_rejected() {
        let params = ListParams {
            top: Some(101),
            skip: None,
            filter: None,
            orderby: None,
            select: None,
        };
        let error = render(Vec::<Value>::new(), &params, 50, 100).expect_err("rejected");
        assert_eq!(error.kind().status(), 400);
    }

    #[test]
    fn select_projects_only_named_fields() {
        let items = vec![serde_json::json!({ "name": "a", "id": "x" })];
        let params = ListParams {
            top: None,
            skip: None,
            filter: None,
            orderby: None,
            select: Some(vec!["name".to_owned()]),
        };
        let page = render(items, &params, 50, 100).expect("render");
        assert_eq!(page.items[0], serde_json::json!({ "name": "a" }));
    }

    #[test]
    fn filter_supports_equality_conjunctions() {
        let value = serde_json::json!({ "enabled": true, "name": "x" });
        assert!(matches_filter(&value, "name eq 'x' and enabled eq true"));
        assert!(!matches_filter(&value, "name eq 'y'"));
        assert!(matches_filter(&value, "enabled eq TRUE"));
        assert!(!matches_filter(&value, "enabled eq false"));
        // A member the item does not carry simply never matches, and a clause
        // without a recognised operator is ignored.
        assert!(!matches_filter(&value, "unknown eq 1"));
        assert!(matches_filter(&value, "startswith(name 'x')"));
    }
}
