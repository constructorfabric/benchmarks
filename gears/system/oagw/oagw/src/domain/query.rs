// Created: 2026-09-02 by Constructor Tech
//! OData `$filter` / `$select` / `$orderby` / `$top` / `$skip` for list endpoints.
//!
//! The supported subset is what `DESIGN.md` §3.3 documents with examples:
//! `alias eq 'api.openai.com'`, `upstream_id eq '{uuid}'`, `type eq 'guard'`,
//! ordering by any field, and field projection. Filtering operates on the
//! serialized resource, so every property the response carries is also
//! filterable and orderable — there is no per-type filter code.

use serde::Serialize;
use serde_json::Value;

use crate::error::GatewayError;

/// The parsed list query string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// `$filter` expression.
    pub filter: Option<String>,
    /// `$select`, split on commas.
    pub select: Option<Vec<String>>,
    /// `$orderby`, as `field [asc|desc]`.
    pub orderby: Option<String>,
    /// `$top`.
    pub top: Option<usize>,
    /// `$skip`.
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Parses the query string into a [`ListQuery`], ignoring unknown keys.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] for a non-numeric `$top`/`$skip`.
    #[must_use]
    pub fn parse(params: &[(String, String)]) -> ListQuery {
        let mut query = ListQuery::default();
        for (key, value) in params {
            match key.as_str() {
                "$filter" => query.filter = Some(value.clone()),
                "$select" => {
                    query.select = Some(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    )
                }
                "$orderby" => query.orderby = Some(value.clone()),
                "$top" => query.top = value.parse().ok(),
                "$skip" => query.skip = value.parse().ok(),
                _ => {}
            }
        }
        query
    }

    /// `true` when no parameter was supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filter.is_none()
            && self.select.is_none()
            && self.orderby.is_none()
            && self.top.is_none()
            && self.skip.is_none()
    }
}

/// Applies the query to `items`, returning the projected page.
///
/// # Errors
///
/// [`GatewayError::Validation`] when the filter or order-by expression is
/// malformed, and when `$top` exceeds the configured maximum.
pub fn apply<T: Serialize>(
    items: &[T],
    query: &ListQuery,
    default_top: usize,
    max_top: usize,
) -> Result<Vec<Value>, GatewayError> {
    let mut values: Vec<Value> = items.iter().map(|i| serde_json::to_value(i).unwrap_or(Value::Null)).collect();

    if let Some(filter) = query.filter.as_deref() {
        let predicate = Filter::parse(filter)?;
        values.retain(|item| predicate.matches(item));
    }

    if let Some(orderby) = query.orderby.as_deref() {
        sort_by(&mut values, orderby)?;
    }

    let top = query.top.unwrap_or(default_top).min(max_top);
    let skip = query.skip.unwrap_or(0);
    values = values.into_iter().skip(skip).take(top).collect();

    if let Some(select) = query.select.as_deref() {
        values = values.into_iter().map(|item| project(item, select)).collect();
    }
    Ok(values)
}

/// One comparison in a `$filter` expression.
struct Filter {
    field: String,
    op: Operator,
    value: Value,
    conj: Option<Box<Filter>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Operator {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "gt" => Some(Self::Gt),
            "ge" => Some(Self::Ge),
            "lt" => Some(Self::Lt),
            "le" => Some(Self::Le),
            _ => None,
        }
    }

    fn holds(self, left: &Value, right: &Value) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Gt => compare(left, right).is_some_and(std::cmp::Ordering::is_gt),
            Self::Ge => compare(left, right).is_some_and(|o| o != std::cmp::Ordering::Less),
            Self::Lt => compare(left, right).is_some_and(std::cmp::Ordering::is_lt),
            Self::Le => compare(left, right).is_some_and(|o| o != std::cmp::Ordering::Greater),
        }
    }
}

/// Numeric-then-lexicographic comparison of two scalar JSON values.
fn compare(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            let a = a.as_f64()?;
            let b = b.as_f64()?;
            a.partial_cmp(&b)
        }
        (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

impl Filter {
    /// Parses `field op 'value' [and field op 'value'] …`.
    fn parse(raw: &str) -> Result<Self, GatewayError> {
        let invalid = || {
            GatewayError::Validation(format!(
                "$filter '{raw}' is not supported; use \"field eq 'value'\" clauses joined by 'and'"
            ))
        };
        // Clauses are left-associated: `a and b and c` becomes a → b → c.
        let mut clauses: Vec<Self> = Vec::new();
        let mut rest = raw.trim();
        loop {
            let (clause, remainder) = Self::parse_clause(rest).ok_or_else(invalid)?;
            clauses.push(clause);
            rest = remainder.trim_start();
            if rest.is_empty() {
                break;
            }
            rest = rest
                .strip_prefix("and")
                .ok_or_else(invalid)?
                .trim_start();
        }
        // Fold the clauses into a right-leaning conjunction chain.
        let mut iter = clauses.into_iter().rev();
        let mut current = iter.next().ok_or_else(invalid)?;
        for clause in iter {
            let mut head = clause;
            head.conj = Some(Box::new(current));
            current = head;
        }
        Ok(current)
    }

    /// Parses a single clause, returning the remaining input.
    fn parse_clause(input: &str) -> Option<(Self, &str)> {
        let mut parts = input.trim().splitn(3, char::is_whitespace);
        let field = parts.next()?.trim().to_owned();
        let op_raw = parts.next()?;
        let op = Operator::parse(op_raw)?;
        let rest = parts.next()?;
        let (value, remainder) = parse_literal(rest)?;
        Some((
            Self {
                field,
                op,
                value,
                conj: None,
            },
            remainder,
        ))
    }

    /// Evaluates the clause (and its conjunction) against `item`.
    fn matches(&self, item: &Value) -> bool {
        let field = item.get(&self.field);
        let holds = match field {
            Some(actual) => self.op.holds(actual, &self.value),
            // A missing field never equals anything; `ne` therefore holds.
            None => self.op == Operator::Ne,
        };
        holds && self.conj.as_ref().is_none_or(|next| next.matches(item))
    }
}

/// Parses a quoted string, a number, or `true`/`false`, returning the remainder.
fn parse_literal(input: &str) -> Option<(Value, &str)> {
    let input = input.trim_start();
    if let Some(rest) = input.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return Some((Value::String(rest[..end].to_owned()), &rest[end + 1..]));
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let (token, rest) = input.split_at(end);
    if token == "true" {
        Some((Value::Bool(true), rest))
    } else if token == "false" {
        Some((Value::Bool(false), rest))
    } else {
        token.parse::<serde_json::Number>().ok().map(|n| (Value::Number(n), rest))
    }
}

/// Sorts by a `$orderby` expression: `field [asc|desc]`.
fn sort_by(values: &mut [Value], raw: &str) -> Result<(), GatewayError> {
    let mut parts = raw.split_whitespace();
    let field = parts.next().ok_or_else(|| bad_orderby(raw))?;
    let desc = match parts.next() {
        None => false,
        Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(bad_orderby(raw)),
    };
    if parts.next().is_some() {
        return Err(bad_orderby(raw));
    }
    values.sort_by(|a, b| {
        let ord = match (a.get(field), b.get(field)) {
            (Some(x), Some(y)) => compare(x, y).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
    Ok(())
}

fn bad_orderby(raw: &str) -> GatewayError {
    GatewayError::Validation(format!("$orderby '{raw}' is not supported; use \"field [asc|desc]\""))
}

/// Keeps only the selected properties of `item`.
fn project(mut item: Value, fields: &[String]) -> Value {
    let Value::Object(map) = &mut item else {
        return item;
    };
    let keys = fields
        .iter()
        .filter_map(|f| {
            map.keys().find(|k| k.eq_ignore_ascii_case(f)).cloned()
        })
        .collect::<Vec<_>>();
    map.retain(|k, _| keys.contains(k));
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn items() -> Vec<Value> {
        vec![
            json!({"alias": "api.openai.com", "enabled": true, "port": 443}),
            json!({"alias": "vendor.com", "enabled": false, "port": 8443}),
            json!({"alias": "us.vendor.com", "enabled": true, "port": 443}),
        ]
    }

    #[test]
    fn an_empty_query_returns_everything_with_the_default_top() {
        let out = apply(&items(), &ListQuery::default(), 2, 100).unwrap();
        assert_eq!(out.len(), 2, "default top is applied");
    }

    #[test]
    fn top_is_capped_at_the_maximum() {
        let query = ListQuery { top: Some(500), ..Default::default() };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn skip_and_top_page_through_the_collection() {
        let query = ListQuery { skip: Some(1), top: Some(1), ..Default::default() };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "vendor.com");
    }

    #[test]
    fn eq_and_ne_filter_on_quoted_values() {
        let query = ListQuery {
            filter: Some("alias eq 'api.openai.com'".to_owned()),
            ..Default::default()
        };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "api.openai.com");

        let query = ListQuery { filter: Some("alias ne 'vendor.com'".to_owned()), ..Default::default() };
        assert_eq!(apply(&items(), &query, 50, 100).unwrap().len(), 2);
    }

    #[test]
    fn filters_combine_with_and_and_support_booleans_and_numbers() {
        let query = ListQuery {
            filter: Some("enabled eq false and port ge 8443".to_owned()),
            ..Default::default()
        };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "vendor.com");
    }

    #[test]
    fn ordering_supports_asc_and_desc() {
        let query = ListQuery { orderby: Some("port desc".to_owned()), ..Default::default() };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out[0]["alias"], "vendor.com");

        let query = ListQuery { orderby: Some("alias asc".to_owned()), ..Default::default() };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out[0]["alias"], "api.openai.com");
    }

    #[test]
    fn select_projects_the_named_fields() {
        let query = ListQuery {
            select: Some(vec!["alias".to_owned()]),
            ..Default::default()
        };
        let out = apply(&items(), &query, 50, 100).unwrap();
        assert_eq!(out[0], json!({"alias": "api.openai.com"}));
    }

    #[test]
    fn malformed_filters_and_orderings_are_rejected() {
        for bad_filter in [
            "alias",
            "alias eq unquoted",
            "alias contains 'x'",
            "alias eq 'x' or alias eq 'y'",
            "",
        ] {
            let query = ListQuery { filter: Some(bad_filter.to_owned()), ..Default::default() };
            assert!(apply(&items(), &query, 50, 100).is_err(), "{bad_filter}");
        }
        let query = ListQuery { orderby: Some("alias sideways".to_owned()), ..Default::default() };
        assert!(apply(&items(), &query, 50, 100).is_err());
    }

    #[test]
    fn filter_parameters_are_read_from_the_query_string() {
        let params = vec![
            ("$filter".to_owned(), "alias eq 'x'".to_owned()),
            ("$select".to_owned(), "alias, port".to_owned()),
            ("$orderby".to_owned(), "port desc".to_owned()),
            ("$top".to_owned(), "5".to_owned()),
            ("$skip".to_owned(), "1".to_owned()),
        ];
        let query = ListQuery::parse(&params);
        assert_eq!(query.filter.as_deref(), Some("alias eq 'x'"));
        assert_eq!(
            query.select.as_deref(),
            Some(&["alias".to_owned(), "port".to_owned()][..])
        );
        assert_eq!(query.orderby.as_deref(), Some("port desc"));
        assert_eq!(query.top, Some(5));
        assert_eq!(query.skip, Some(1));
        assert!(!query.is_empty());
        assert!(ListQuery::default().is_empty());
    }
}
