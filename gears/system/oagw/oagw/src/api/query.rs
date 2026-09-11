//! OData-style list query parameters.
//!
//! The platform's OData extractor rejects `$skip`, so the gear parses
//! `$filter`, `$select`, `$orderby`, `$top` and `$skip` itself.

use crate::domain::error::OagwError;

/// Default page size.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size.
pub const MAX_TOP: usize = 100;

/// Comparison operator of a filter clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// Equal.
    Equal,
    /// Not equal.
    NotEqual,
    /// Greater than.
    Greater,
    /// Greater or equal.
    GreaterOrEqual,
    /// Less than.
    Less,
    /// Less or equal.
    LessOrEqual,
}

impl FilterOp {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "eq" => Some(Self::Equal),
            "ne" => Some(Self::NotEqual),
            "gt" => Some(Self::Greater),
            "ge" => Some(Self::GreaterOrEqual),
            "lt" => Some(Self::Less),
            "le" => Some(Self::LessOrEqual),
            _ => None,
        }
    }

    fn apply(self, ordering: Option<std::cmp::Ordering>) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Self::Equal => ordering == Some(Equal),
            Self::NotEqual => ordering != Some(Equal),
            Self::Greater => matches!(ordering, Some(Greater)),
            Self::GreaterOrEqual => matches!(ordering, Some(Greater) | Some(Equal)),
            Self::Less => matches!(ordering, Some(Less)),
            Self::LessOrEqual => matches!(ordering, Some(Less) | Some(Equal)),
        }
    }
}

/// A parsed list query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// AND-combined filter predicates, evaluated on the projected resource.
    pub filter: Vec<(String, FilterOp, serde_json::Value)>,
    /// Fields to project; empty means the full resource.
    pub select: Vec<String>,
    /// Sort keys with a descending flag.
    pub orderby: Vec<(String, bool)>,
    /// Page size.
    pub top: usize,
    /// Page offset.
    pub skip: usize,
}

impl ListQuery {
    /// Parses the query parameters of a list request.
    ///
    /// # Errors
    ///
    /// 400 for a malformed or unsupported parameter.
    pub fn parse(params: &std::collections::HashMap<String, String>) -> Result<Self, OagwError> {
        let mut query = Self {
            top: DEFAULT_TOP,
            ..Self::default()
        };

        for (key, value) in params {
            match key.as_str() {
                "$filter" => query.filter = parse_filter(value)?,
                "$select" => query.select = parse_select(value)?,
                "$orderby" => query.orderby = parse_orderby(value)?,
                "$top" => query.top = parse_limit(value, "$top")?,
                "$skip" => query.skip = parse_limit(value, "$skip")?,
                other if other.starts_with('$') => {
                    return Err(OagwError::validation(format!(
                        "unsupported query option {key:?}"
                    )));
                }
                _ => {}
            }
        }
        Ok(query)
    }

    /// Applies the query to a set of resources already serialized as JSON.
    #[must_use]
    pub fn apply(&self, items: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        let mut items = items
            .into_iter()
            .filter(|item| self.matches(item))
            .collect::<Vec<_>>();
        if !self.orderby.is_empty() {
            items.sort_by(|a, b| {
                for (field, descending) in &self.orderby {
                    let ordering = compare(&field_value(a, field), &field_value(b, field));
                    // An incomparable pair (mismatched types) sorts as equal.
                    let ordering = ordering.unwrap_or(std::cmp::Ordering::Equal);
                    let ordering = if *descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        items
            .into_iter()
            .skip(self.skip)
            .take(self.top)
            .map(|item| self.project(item))
            .collect()
    }

    /// Whether an item passes every filter term.
    fn matches(&self, item: &serde_json::Value) -> bool {
        self.filter.iter().all(|(field, op, expected)| {
            let actual = field_value(item, field);
            if actual.is_null() {
                return *op == FilterOp::NotEqual;
            }
            op.apply(compare(&actual, expected))
        })
    }

    /// Reduces an item to the `$select`ed members.
    fn project(&self, item: serde_json::Value) -> serde_json::Value {
        if self.select.is_empty() {
            return item;
        }
        let Some(object) = item.as_object() else {
            return item;
        };
        let mut projected = serde_json::Map::new();
        for field in &self.select {
            if let Some(value) = object.get(field) {
                projected.insert(field.clone(), value.clone());
            }
        }
        serde_json::Value::Object(projected)
    }
}

/// Reads a member, descending into one level of a nested `field.sub` path.
fn field_value(item: &serde_json::Value, field: &str) -> serde_json::Value {
    let Some((head, rest)) = field.split_once('.') else {
        return item.get(field).cloned().unwrap_or(serde_json::Value::Null);
    };
    item.get(head)
        .and_then(|value| value.get(rest))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Total order over JSON scalars, used by `$orderby` and `$filter`.
fn compare(a: &serde_json::Value, b: &serde_json::Value) -> Option<std::cmp::Ordering> {
    use serde_json::Value::*;
    match (a, b) {
        (Null, Null) => Some(std::cmp::Ordering::Equal),
        (Null, _) => Some(std::cmp::Ordering::Less),
        (_, Null) => Some(std::cmp::Ordering::Greater),
        (Bool(x), Bool(y)) => x.partial_cmp(y),
        (Number(x), Number(y)) => x.as_f64().partial_cmp(&y.as_f64()),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Array(x), Array(y)) => {
            for (a, b) in x.iter().zip(y.iter()) {
                match compare(a, b) {
                    Some(std::cmp::Ordering::Equal) => continue,
                    other => return other,
                }
            }
            x.len().partial_cmp(&y.len())
        }
        _ => None,
    }
}

/// One parsed `$filter` term: field, operator and literal.
type FilterTerm = (String, FilterOp, serde_json::Value);

/// Parses `$filter` as a conjunction of `field op value` terms.
fn parse_filter(raw: &str) -> Result<Vec<FilterTerm>, OagwError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut terms = Vec::new();
    for term in raw.split(" and ") {
        let term = term.trim();
        let Some((field, rest)) = term.split_once(' ') else {
            return Err(bad_filter(term));
        };
        let rest = rest.trim_start();
        let Some((op, value)) = rest.split_once(' ') else {
            return Err(bad_filter(term));
        };
        let Some(op) = FilterOp::parse(op.trim()) else {
            return Err(OagwError::validation(format!(
                "unsupported operator in $filter term {term:?}; expected eq, ne, gt, ge, lt or le"
            )));
        };
        terms.push((field.trim().to_owned(), op, parse_value(value.trim())?));
    }
    Ok(terms)
}

fn bad_filter(term: &str) -> OagwError {
    OagwError::validation(format!(
        "malformed $filter term {term:?}; expected `field op value`"
    ))
}

/// Parses a filter literal: `'text'`, a number, `true`, `false` or `null`.
fn parse_value(raw: &str) -> Result<serde_json::Value, OagwError> {
    if let Some(rest) = raw.strip_prefix('\'') {
        let Some(text) = rest.strip_suffix('\'') else {
            return Err(OagwError::validation(format!(
                "unterminated string literal in $filter: {raw:?}"
            )));
        };
        return Ok(serde_json::Value::from(text.replace("''", "'")));
    }
    if raw == "null" {
        return Ok(serde_json::Value::Null);
    }
    if raw == "true" {
        return Ok(serde_json::Value::Bool(true));
    }
    if raw == "false" {
        return Ok(serde_json::Value::Bool(false));
    }
    raw.parse::<i64>()
        .map(serde_json::Value::from)
        .map_err(|_| OagwError::validation(format!("unsupported literal in $filter: {raw:?}")))
}

/// Parses `$select` as a comma-separated field list.
fn parse_select(raw: &str) -> Result<Vec<String>, OagwError> {
    let fields: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if fields.is_empty() {
        return Err(OagwError::validation(
            "$select must name at least one field",
        ));
    }
    Ok(fields)
}

/// Parses `$orderby` as `field [asc|desc] [, ...]`.
fn parse_orderby(raw: &str) -> Result<Vec<(String, bool)>, OagwError> {
    let mut keys = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut words = part.split_whitespace();
        let Some(field) = words.next() else {
            continue;
        };
        let descending = match words.next() {
            None => false,
            Some(dir) if dir.eq_ignore_ascii_case("asc") => false,
            Some(dir) if dir.eq_ignore_ascii_case("desc") => true,
            Some(dir) => {
                return Err(OagwError::validation(format!(
                    "invalid sort direction {dir:?} in $orderby"
                )));
            }
        };
        keys.push((field.to_owned(), descending));
    }
    Ok(keys)
}

/// Parses `$top` / `$skip`.
fn parse_limit(raw: &str, name: &str) -> Result<usize, OagwError> {
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| OagwError::validation(format!("{name} must be a non-negative integer")))?;
    if value < 0 {
        return Err(OagwError::validation(format!(
            "{name} must be a non-negative integer"
        )));
    }
    if name == "$top" && value as usize > MAX_TOP {
        return Err(OagwError::validation(format!(
            "{name} must not exceed {MAX_TOP}"
        )));
    }
    Ok(value as usize)
}
