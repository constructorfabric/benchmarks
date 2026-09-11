//! The `OData` subset the management list endpoints accept.
//!
//! `$top` (default 50, cap 100), `$skip`, `$orderby=field [asc|desc]`,
//! `$filter=field eq 'value'` and `$select=field,field` are parsed from the
//! query string and applied in-process.

use serde_json::Value;

/// The parsed list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// Field to sort on and its direction.
    pub orderby: Option<(String, bool)>,
    /// `field eq 'value'` filter.
    pub filter: Option<(String, String)>,
    /// Fields the caller asked for.
    pub select: Vec<String>,
    /// Number of records to skip.
    pub skip: usize,
    /// Number of records to return.
    pub top: usize,
}

/// Default page size.
pub const DEFAULT_TOP: usize = 50;

/// Maximum page size.
pub const MAX_TOP: usize = 100;

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            orderby: None,
            filter: None,
            select: Vec::new(),
            skip: 0,
            top: DEFAULT_TOP,
        }
    }
}

impl ListQuery {
    /// Applies `skip`/`top` to an ordered list.
    #[must_use]
    pub fn page<T>(&self, items: Vec<T>) -> Vec<T> {
        items.into_iter().skip(self.skip).take(self.top).collect()
    }

    /// Applies `$orderby` and `$filter` to a list of JSON documents.
    #[must_use]
    pub fn apply(&self, items: Vec<Value>) -> Vec<Value> {
        let filtered: Vec<Value> = match &self.filter {
            Some((field, expected)) => items
                .into_iter()
                .filter(|item| {
                    item.get(field)
                        .and_then(Value::as_str)
                        .is_some_and(|actual| actual == expected)
                })
                .collect(),
            None => items,
        };
        let mut filtered = filtered;
        if let Some((field, descending)) = &self.orderby {
            let key = |item: &Value| -> String {
                item.get(field).map_or_else(String::new, |value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
            };
            filtered.sort_by_key(|left| key(left));
            if *descending {
                filtered.reverse();
            }
        }
        self.page(filtered)
    }

    /// Projects each document onto `$select`'s fields.
    #[must_use]
    pub fn project(&self, items: Vec<Value>) -> Vec<Value> {
        if self.select.is_empty() {
            return items;
        }
        items
            .into_iter()
            .map(|item| {
                let mut projected = serde_json::Map::new();
                for field in &self.select {
                    if let Some(value) = item.get(field) {
                        projected.insert(field.clone(), value.clone());
                    }
                }
                Value::Object(projected)
            })
            .collect()
    }
}

/// Parses `$orderby=field [asc|desc]`.
#[must_use]
pub fn parse_orderby(raw: &str) -> Option<(String, bool)> {
    let mut parts = raw.split_whitespace();
    let field = parts.next()?.to_owned();
    let descending = match parts.next() {
        None => false,
        Some(dir) => dir.eq_ignore_ascii_case("desc"),
    };
    Some((field, descending))
}

/// Parses `$filter=field eq 'value'`.
#[must_use]
pub fn parse_filter(raw: &str) -> Option<(String, String)> {
    let mut parts = raw.trim().splitn(3, ' ');
    let field = parts.next()?.to_owned();
    let operator = parts.next()?;
    if !operator.eq_ignore_ascii_case("eq") {
        return None;
    }
    let value = parts.next()?;
    Some((field, value.trim_matches('\'').to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(alias: &str, enabled: bool) -> Value {
        serde_json::json!({ "alias": alias, "enabled": enabled })
    }

    #[test]
    fn top_and_skip_slice_the_page() {
        let query = ListQuery {
            skip: 1,
            top: 2,
            ..ListQuery::default()
        };
        let items = query.apply(vec![
            item("a", true),
            item("b", true),
            item("c", true),
            item("d", true),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["alias"], "b");
    }

    #[test]
    fn filter_narrows_results() {
        let query = ListQuery {
            filter: Some(("alias".to_owned(), "b".to_owned())),
            ..ListQuery::default()
        };
        let items = query.apply(vec![item("a", true), item("b", true)]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["alias"], "b");
    }

    #[test]
    fn orderby_sorts_both_ways() {
        let ascending = ListQuery {
            orderby: Some(("alias".to_owned(), false)),
            ..ListQuery::default()
        };
        let descending = ListQuery {
            orderby: Some(("alias".to_owned(), true)),
            ..ListQuery::default()
        };
        let source = vec![item("c", true), item("a", true), item("b", true)];
        let up = ascending.apply(source.clone());
        assert_eq!(up[0]["alias"], "a");
        let down = descending.apply(source);
        assert_eq!(down[0]["alias"], "c");
    }

    #[test]
    fn select_projects_fields() {
        let query = ListQuery {
            select: vec!["alias".to_owned()],
            ..ListQuery::default()
        };
        let items = query.project(vec![item("a", true)]);
        assert_eq!(items[0]["alias"], "a");
        assert!(items[0].get("enabled").is_none());
    }

    #[test]
    fn parse_orderby_reads_the_direction() {
        assert_eq!(parse_orderby("alias desc"), Some(("alias".to_owned(), true)));
        assert_eq!(parse_orderby("alias"), Some(("alias".to_owned(), false)));
    }

    #[test]
    fn parse_filter_accepts_eq_only() {
        assert_eq!(
            parse_filter("alias eq 'x'"),
            Some(("alias".to_owned(), "x".to_owned()))
        );
        assert!(parse_filter("alias ne 'x'").is_none());
    }
}
