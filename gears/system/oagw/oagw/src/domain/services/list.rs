//! Minimal `OData` listing support for the OAGW management plane.
//!
//! The docs expose `$filter`, `$select`, `$orderby`, `$top`, `$skip`.
//! This slice implements the parts the acceptance surface exercises:
//! `$top` / `$skip` pagination and equality filters over the wire fields
//! (`$filter=alias eq 'x'`, `$filter=upstream_id eq '{uuid}'`,
//! `$filter=type eq 'guard'`). Unknown clauses are ignored rather than
//! rejected so forward-compatible clients keep working.

use std::collections::HashMap;

/// Parsed listing query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Equalities `field -> value` (AND-combined).
    pub equals: HashMap<String, String>,
    /// `$top` (default 50, max 100 per DOCS).
    pub top: usize,
    /// `$skip`.
    pub skip: usize,
}

impl ListQuery {
    /// Parse the raw query string (without the leading `?`).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut query = ListQuery::default();
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "$top" => {
                    if let Ok(n) = value.parse::<usize>() {
                        query.top = n.min(MAX_TOP);
                    }
                }
                "$skip" => {
                    if let Ok(n) = value.parse::<usize>() {
                        query.skip = n;
                    }
                }
                "$filter" => {
                    if let Some((field, val)) = parse_filter(&value) {
                        query.equals.insert(field, strip_quotes(&val));
                    }
                }
                _ => {}
            }
        }
        if query.top == 0 {
            query.top = DEFAULT_TOP;
        }
        query
    }
}

/// Max page size (DOCS: `$top` default 50, max 100).
pub const MAX_TOP: usize = 100;
/// Default page size.
pub const DEFAULT_TOP: usize = 50;

/// Parse a single `{field} eq {value}` comparison. Returns `None` for
/// anything else.
fn parse_filter(expr: &str) -> Option<(String, String)> {
    let mut parts = expr.splitn(3, char::is_whitespace);
    let field = parts.next()?.trim();
    let op = parts.next()?.trim();
    let value = parts.next()?.trim();
    if field.is_empty() || value.is_empty() || !op.eq_ignore_ascii_case("eq") {
        return None;
    }
    Some((field.to_owned(), value.to_owned()))
}

fn strip_quotes(value: &str) -> String {
    value
        .trim_matches(|c| c == '\'' || c == '"')
        .to_owned()
}

/// Apply the filter + pagination to a JSON-encoded item list (each item
/// is the wire shape, e.g. `serde_json::to_value(upstream)`).
///
/// Items whose wire value has a field matching an `eq` clause survive;
/// a missing field never matches. Pagination applies `skip` then `top`.
#[must_use]
pub fn apply<T>(
    items: Vec<T>,
    query: &ListQuery,
    to_wire: impl Fn(&T) -> serde_json::Value,
) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        let wire = to_wire(&item);
        let matches = query.equals.iter().all(|(field, want)| {
            wire.get(field)
                .and_then(|v| v.as_str())
                .is_some_and(|got| got.eq_ignore_ascii_case(want))
        });
        if matches {
            out.push(item);
        }
    }
    if query.skip > 0 {
        out = out.into_iter().skip(query.skip).collect();
    }
    out.truncate(query.top);
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parse_top_defaults_and_max() {
        let q = ListQuery::parse("$top=1000");
        assert_eq!(q.top, MAX_TOP);
        let q = ListQuery::parse("");
        assert_eq!(q.top, DEFAULT_TOP);
        let q = ListQuery::parse("$top=20&$skip=5");
        assert_eq!(q.top, 20);
        assert_eq!(q.skip, 5);
    }

    #[test]
    fn parse_equality_filter() {
        let q = ListQuery::parse("$filter=alias eq 'api.openai.com'");
        assert_eq!(q.equals.get("alias").map(String::as_str), Some("api.openai.com"));
    }

    #[test]
    fn apply_filters_and_paginates() {
        let to_wire = |i: &serde_json::Value| i.clone();
        let items = vec![
            serde_json::json!({"alias": "a", "tags": []}),
            serde_json::json!({"alias": "b", "tags": []}),
            serde_json::json!({"alias": "a", "tags": []}),
        ];
        let q = ListQuery::parse("$filter=alias eq 'a'&$top=1");
        let out = apply(items, &q, to_wire);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "a");
    }
}
