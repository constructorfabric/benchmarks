//! Internal domain types shared by the control plane services.
//!
//! These are *not* wire DTOs; the REST layer ([`crate::api::rest`]) converts
//! its own request/response shapes to and from them.

use serde::Deserialize;
use serde_json::Value;

use crate::domain::error::DomainError;

/// Sort direction of an `$orderby` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    /// Ascending.
    #[default]
    Asc,
    /// Descending.
    Desc,
}

/// A management-API list query.
///
/// Implements the OData-flavoured subset the DESIGN.md management API table
/// documents: `$filter` (comparisons combined with `and`/`or`), `$orderby`,
/// `$select`, `$top` and `$skip`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// Raw `$filter` expression.
    pub filter: Option<String>,
    /// Field names for `$select` (lower-cased, in request order).
    pub select: Vec<String>,
    /// Raw `$orderby` expression.
    pub orderby: Option<String>,
    /// Clamped page size.
    pub top: u64,
    /// Offset into the ordered result set.
    pub skip: u64,
    /// Sort direction, when the expression carries no explicit one.
    pub order_dir: SortDir,
    /// Configured maximum page size.
    pub max_top: u64,
}

impl ListQuery {
    /// Build a list query, clamping `top` to `max_top`.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when `$top`/`$skip` are not valid
    /// unsigned integers or when the field lists are malformed.
    pub fn new(
        filter: Option<String>,
        select: Option<String>,
        orderby: Option<String>,
        top: Option<u64>,
        skip: Option<u64>,
        default_top: u64,
        max_top: u64,
    ) -> Result<Self, DomainError> {
        let top = match top {
            None => default_top,
            Some(0) => default_top,
            Some(v) => v.min(max_top.max(1)),
        };
        let skip = skip.unwrap_or(0);
        let select = match select {
            None => Vec::new(),
            Some(raw) => raw
                .split(',')
                .map(|f| f.trim().to_ascii_lowercase())
                .filter(|f| !f.is_empty())
                .collect(),
        };
        if !select.is_empty() && select.iter().any(|f| f.contains(char::is_whitespace)) {
            return Err(DomainError::validation(
                "$select",
                "field list must be comma-separated field names",
            ));
        }
        if let Some(raw) = orderby.as_deref() {
            for clause in raw.split(',') {
                let clause = clause.trim();
                let field = clause.split_whitespace().next().unwrap_or_default();
                if field.is_empty() {
                    return Err(DomainError::validation("$orderby", "empty sort field"));
                }
            }
        }
        Ok(Self {
            filter,
            select,
            orderby,
            top,
            skip,
            order_dir: SortDir::default(),
            max_top,
        })
    }

    /// Build from the raw management-API query parameters.
    ///
    /// # Errors
    /// See [`ListQuery::new`].
    pub fn from_parts(
        params: &ListParams,
        default_top: u64,
        max_top: u64,
    ) -> Result<Self, DomainError> {
        let mut q = Self::new(
            params.filter.clone(),
            params.select.clone(),
            params.orderby.clone(),
            params.top,
            params.skip,
            default_top,
            max_top,
        )?;
        q.order_dir = params.order_dir;
        Ok(q)
    }

    /// Field to sort by, when requested.
    #[must_use]
    pub fn order_field(&self) -> Option<&str> {
        self.orderby
            .as_deref()
            .and_then(|raw| raw.split(',').next())
            .map(|clause| clause.split_whitespace().next().unwrap_or_default())
            .filter(|f| !f.is_empty())
    }

    /// Effective sort direction for the primary sort field.
    #[must_use]
    pub fn order_direction(&self) -> SortDir {
        let Some(raw) = self.orderby.as_deref() else {
            return SortDir::default();
        };
        let Some(clause) = raw.split(',').next() else {
            return SortDir::default();
        };
        if clause.to_ascii_lowercase().split_whitespace().nth(1) == Some("desc") {
            SortDir::Desc
        } else {
            SortDir::Asc
        }
    }

    /// True when no `$filter` is present.
    #[must_use]
    pub fn is_unfiltered(&self) -> bool {
        self.filter.as_deref().is_none_or(str::is_empty)
    }

    /// Evaluate the `$filter` expression against a serialised resource.
    #[must_use]
    pub fn matches(&self, value: &Value) -> bool {
        if self.is_unfiltered() {
            return true;
        }
        let raw = self.filter.as_deref().unwrap_or_default();
        parse_or_group(raw).is_all(|c| c.is_all(|atom| atom.matches(value)))
    }

    /// Apply filter → order → skip → top to already-serialised resources.
    #[must_use]
    pub fn apply(&self, mut items: Vec<Value>) -> Vec<Value> {
        items.retain(|item| self.matches(item));
        if let Some(field) = self.order_field() {
            let dir = self.order_direction();
            items.sort_by(|a, b| {
                let ord = json_cmp(&lookup(a, field), &lookup(b, field));
                if dir == SortDir::Desc {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        items
            .into_iter()
            .skip(usize::try_from(self.skip).unwrap_or(usize::MAX))
            .take(usize::try_from(self.top).unwrap_or(usize::MAX))
            .collect()
    }

    /// Total number of items matching the filter, before pagination.
    #[must_use]
    pub fn filtered_count(&self, items: &[Value]) -> usize {
        items.iter().filter(|item| self.matches(item)).count()
    }
}

/// Raw management-API list parameters, before clamping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ListParams {
    /// `$filter`
    #[serde(default, rename = "$filter")]
    pub filter: Option<String>,
    /// `$select`
    #[serde(default, rename = "$select")]
    pub select: Option<String>,
    /// `$orderby`
    #[serde(default, rename = "$orderby")]
    pub orderby: Option<String>,
    /// `$top`
    #[serde(default, rename = "$top", alias = "limit")]
    pub top: Option<u64>,
    /// `$skip`
    #[serde(default, rename = "$skip", alias = "offset")]
    pub skip: Option<u64>,
    /// Explicit sort direction, when not embedded in `$orderby`.
    #[serde(default, skip)]
    pub order_dir: SortDir,
}

/// A single `$filter` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterAtom {
    field: String,
    op: FilterOp,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOp {
    Eq,
    Ne,
}

impl FilterAtom {
    fn matches(&self, value: &Value) -> bool {
        let actual = lookup(value, &self.field);
        let expected = literal(&self.value);
        match self.op {
            FilterOp::Eq => json_eq(&actual, &expected),
            FilterOp::Ne => !json_eq(&actual, &expected),
        }
    }
}

/// A group of comparisons joined by `and`.
struct FilterConjunction(Vec<FilterAtom>);

impl FilterConjunction {
    fn is_all(&self, mut f: impl FnMut(&FilterAtom) -> bool) -> bool {
        self.0.iter().all(&mut f)
    }
}

/// A group of conjunctions joined by `or`.
struct FilterDisjunction(Vec<FilterConjunction>);

impl FilterDisjunction {
    fn is_all(&self, mut f: impl FnMut(&FilterConjunction) -> bool) -> bool {
        if self.0.is_empty() {
            return true;
        }
        self.0.iter().any(&mut f)
    }
}

fn parse_or_group(raw: &str) -> FilterDisjunction {
    let mut groups = Vec::new();
    for part in split_ci(raw, " or ") {
        let mut atoms = Vec::new();
        for clause in split_ci(&part, " and ") {
            if let Some(atom) = parse_atom(&clause) {
                atoms.push(atom);
            }
        }
        groups.push(FilterConjunction(atoms));
    }
    FilterDisjunction(groups)
}

/// Split on a case-insensitive delimiter.
fn split_ci(input: &str, sep: &str) -> Vec<String> {
    split_literal(input, sep)
}

fn split_literal(input: &str, sep: &str) -> Vec<String> {
    let lower = input.to_ascii_lowercase();
    let sep_lower = sep.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    let bytes = lower.as_bytes();
    while cursor + sep.len() <= bytes.len() {
        if lower[cursor..cursor + sep.len()] == sep_lower {
            out.push(input[start..cursor].to_owned());
            cursor += sep.len();
            start = cursor;
        } else {
            cursor += 1;
        }
    }
    out.push(input[start..].to_owned());
    out
}

fn parse_atom(clause: &str) -> Option<FilterAtom> {
    let clause = clause.trim();
    for (needle, op) in [(" ne ", FilterOp::Ne), (" eq ", FilterOp::Eq)] {
        if let Some(pos) = find_ci(clause, needle) {
            let field = clause[..pos].trim().trim_matches('\'').to_owned();
            let value = clause[pos + needle.len()..].trim().to_owned();
            if field.is_empty() || value.is_empty() {
                return None;
            }
            return Some(FilterAtom { field, op, value });
        }
    }
    None
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

/// Interpret a filter literal: quoted string, boolean, number or `null`.
fn literal(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')))
    {
        return Value::String(trimmed[1..trimmed.len() - 1].to_owned());
    }
    match trimmed {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => trimmed
            .parse::<f64>()
            .map(|n| serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
            .unwrap_or_else(|_| Value::String(trimmed.to_owned())),
    }
}

fn json_eq(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .is_some_and(|(x, y)| (x - y).abs() < f64::EPSILON),
        (Value::String(a), Value::String(b)) => a.eq_ignore_ascii_case(b),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_eq(x, y))
        }
        _ => false,
    }
}

/// Case-insensitive dot-path lookup on a serialised resource.
fn lookup(value: &Value, path: &str) -> Value {
    let mut current = value;
    for segment in path.split('.') {
        match current {
            Value::Object(map) => {
                let found = map
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(segment))
                    .map(|(_, v)| v);
                match found {
                    Some(v) => current = v,
                    None => return Value::Null,
                }
            }
            Value::Array(arr) => match segment.parse::<usize>() {
                Ok(idx) if idx < arr.len() => current = &arr[idx],
                _ => return Value::Null,
            },
            _ => return Value::Null,
        }
    }
    current.clone()
}

fn json_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .zip(y.as_f64())
            .map_or(Ordering::Equal, |(p, q)| {
                p.partial_cmp(&q).unwrap_or(Ordering::Equal)
            }),
        (Value::String(x), Value::String(y)) => x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

/// Compare two JSON values for `$orderby`.
#[must_use]
pub fn compare_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    json_cmp(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, enabled: bool, alias: &str) -> Value {
        serde_json::json!({"id": id, "enabled": enabled, "alias": alias})
    }

    #[test]
    fn filter_eq_and_ne() {
        let q = ListQuery::new(
            Some("enabled eq true".to_owned()),
            None,
            None,
            None,
            None,
            50,
            100,
        )
        .unwrap();
        assert!(q.matches(&item("a", true, "x")));
        assert!(!q.matches(&item("b", false, "y")));
    }

    #[test]
    fn filter_and_or() {
        let q = ListQuery::new(
            Some("enabled eq true and alias eq 'vendor.com'".to_owned()),
            None,
            None,
            None,
            None,
            50,
            100,
        )
        .unwrap();
        assert!(q.matches(&item("a", true, "vendor.com")));
        assert!(!q.matches(&item("a", true, "other.com")));
        assert!(!q.matches(&item("a", false, "vendor.com")));

        let q = ListQuery::new(
            Some("alias eq 'a.com' or alias eq 'b.com'".to_owned()),
            None,
            None,
            None,
            None,
            50,
            100,
        )
        .unwrap();
        assert!(q.matches(&item("a", true, "b.com")));
        assert!(!q.matches(&item("a", true, "c.com")));
    }

    #[test]
    fn ordering_and_pagination() {
        let q = ListQuery::new(
            None,
            None,
            Some("alias desc".to_owned()),
            Some(2),
            Some(1),
            50,
            100,
        )
        .unwrap();
        let out = q.apply(vec![
            item("1", true, "aaa.com"),
            item("2", true, "ccc.com"),
            item("3", true, "bbb.com"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["alias"], "bbb.com");
        assert_eq!(out[1]["alias"], "aaa.com");
    }

    #[test]
    fn top_is_clamped() {
        let q = ListQuery::new(None, None, None, Some(1_000), None, 50, 100).unwrap();
        assert_eq!(q.top, 100);
        let q = ListQuery::new(None, None, None, Some(0), None, 50, 100).unwrap();
        assert_eq!(q.top, 50);
    }
}
