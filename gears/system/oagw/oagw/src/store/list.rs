//! `OData` system query options over the store's tables.
//!
//! `$top` defaults to 50 and is capped at 100; a negative `$skip` is a validation
//! error rather than an unbounded result. The platform's shared `OData` extractor
//! rejects `$skip` outright, so oagw parses its own options to honour the component
//! contract.

use serde_json::Value;

use crate::error::{ErrorKind, OagwError};

/// Default page size when `$top` is absent.
pub const DEFAULT_TOP: u64 = 50;
/// Maximum page size `$top` may request.
pub const MAX_TOP: u64 = 100;

/// Parsed list query options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Raw `$filter` expression, if any.
    pub filter: Option<String>,
    /// Field names to project, if any.
    pub select: Option<Vec<String>>,
    /// Ordering clauses, in order.
    pub orderby: Vec<(String, bool)>,
    /// Page size.
    pub top: u64,
    /// Offset.
    pub skip: u64,
}

impl ListQuery {
    /// Parses the query string of a request.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a malformed `$top`/`$skip`, an out-of-range
    /// `$top`, an unsupported `$orderby` field or an unknown system option.
    pub fn parse(query: &str) -> Result<Self, OagwError> {
        let mut this = Self {
            top: DEFAULT_TOP,
            ..Self::default()
        };

        for pair in form_urlencoded::parse(query.as_bytes()) {
            let (key, value) = (pair.0.to_string(), pair.1.to_string());
            match key.as_str() {
                "$top" | "limit" => {
                    let parsed: i64 = value.parse().map_err(|_| {
                        OagwError::new(ErrorKind::ValidationError, "$top must be an integer")
                    })?;
                    if parsed < 0 {
                        return Err(OagwError::new(
                            ErrorKind::ValidationError,
                            "$top must not be negative",
                        ));
                    }
                    // Clamp rather than return unbounded results.
                    this.top = u64::try_from(parsed).unwrap_or(MAX_TOP).min(MAX_TOP);
                }
                "$skip" | "offset" => {
                    let parsed: i64 = value.parse().map_err(|_| {
                        OagwError::new(ErrorKind::ValidationError, "$skip must be an integer")
                    })?;
                    if parsed < 0 {
                        return Err(OagwError::new(
                            ErrorKind::ValidationError,
                            "$skip must not be negative",
                        ));
                    }
                    this.skip = u64::try_from(parsed).unwrap_or_default();
                }
                "$filter" => this.filter = Some(value.clone()),
                "$select" => {
                    let fields: Vec<String> = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if fields.is_empty() {
                        return Err(OagwError::new(
                            ErrorKind::ValidationError,
                            "$select must name at least one field",
                        ));
                    }
                    this.select = Some(fields);
                }
                "$orderby" => {
                    for term in value.split(',') {
                        let term = term.trim();
                        let (field, ascending) = match term.rsplit_once(' ') {
                            Some((f, "asc")) => (f.trim().to_owned(), true),
                            Some((f, "desc")) => (f.trim().to_owned(), false),
                            _ => (term.to_owned(), true),
                        };
                        if field.is_empty() {
                            return Err(OagwError::new(
                                ErrorKind::ValidationError,
                                "$orderby field must not be empty",
                            ));
                        }
                        this.orderby.push((field, ascending));
                    }
                }
                k if k.starts_with('$') => {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        format!("unsupported system query option `{k}`"),
                    ));
                }
                _ => {}
            }
        }
        Ok(this)
    }

    /// Applies `$orderby`, `$skip` and `$top` to a list of JSON values.
    #[must_use]
    pub fn apply(&self, mut items: Vec<Value>) -> Vec<Value> {
        for (field, ascending) in &self.orderby {
            items.sort_by(|a, b| {
                let av = a.get(field).cloned().unwrap_or(Value::Null);
                let bv = b.get(field).cloned().unwrap_or(Value::Null);
                let ordering = compare_json(&av, &bv);
                if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            });
        }
        items
            .into_iter()
            .skip(usize::try_from(self.skip).unwrap_or(usize::MAX))
            .take(usize::try_from(self.top).unwrap_or(usize::MAX))
            .collect()
    }

    /// Applies `$filter` as a conjunction of `field eq value` terms.
    #[must_use]
    pub fn filter(&self, items: Vec<Value>) -> Vec<Value> {
        let Some(expression) = &self.filter else {
            return items;
        };
        let predicates = parse_filter(expression);
        items
            .into_iter()
            .filter(|item| {
                predicates.iter().all(|(field, expected)| {
                    // A field that is absent is indistinguishable from a null field.
                    json_matches(item.get(field).unwrap_or(&Value::Null), expected)
                })
            })
            .collect()
    }
}

/// Splits `$filter` into `(field, expected)` equality terms.
fn parse_filter(expression: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for term in expression.split(" and ") {
        let term = term.trim();
        if let Some((field, value)) = term.split_once(" eq ") {
            let field = field.trim().trim_matches('"').to_owned();
            let value = value.trim();
            let parsed = if value == "null" {
                Value::Null
            } else if let Ok(b) = value.parse::<bool>() {
                Value::Bool(b)
            } else if let Ok(n) = value.parse::<i64>() {
                Value::Number(n.into())
            } else {
                Value::String(value.trim_matches('\'').to_owned())
            };
            out.push((field, parsed));
        }
    }
    out
}

/// Whether a JSON value matches a filter literal.
fn json_matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Null, Value::Null) => true,
        (Value::String(a), Value::String(b)) => a.eq_ignore_ascii_case(b),
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        _ => false,
    }
}

/// Total ordering across JSON values for `$orderby`.
fn compare_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&b.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
