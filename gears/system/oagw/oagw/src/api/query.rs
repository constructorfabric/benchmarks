// Created: 2026-08-31 by Constructor Tech
//! List query pipeline (DESIGN §3.3 "List Query Parameters").
//!
//! The platform `OData` extractor binds `$top`/`$skiptoken` cursor paging and
//! rejects `$skip`, while OAGW's contract is offset paging (`$top`, `$skip`).
//! The system options are therefore bound here, minimally: `eq` filters,
//! `$orderby`, `$select`, `$top` (default 50, max 100) and `$skip`.
//!
//! Items are handled as JSON values so one pipeline serves upstreams, routes
//! and plugins.

use serde_json::Value;
use toolkit::Page;
use toolkit::api::odata::parse_orderby;
use toolkit::api::odata::parse_select;
use toolkit::api::select::project_json;
use toolkit_odata::SortDir;

use crate::error::{OagwError, OagwResult};

/// Default page size (DESIGN §3.3).
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size (DESIGN §3.3).
pub const MAX_TOP: usize = 100;

/// A parsed list request.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// `field eq 'value'`, when a filter was sent.
    pub filter: Option<(String, String)>,
    /// Projected field names, lowercased.
    pub select: Option<Vec<String>>,
    /// `(field, descending)` in declaration order.
    pub orderby: Vec<(String, bool)>,
    /// Page size, already clamped to [`MAX_TOP`].
    pub top: usize,
    /// Offset.
    pub skip: usize,
}

impl ListQuery {
    /// Parse the query pairs of a list request.
    ///
    /// # Errors
    /// 400 on an unparsable `$top`/`$skip`, a page size above [`MAX_TOP`], a
    /// filter operator other than `eq`, or an invalid `$orderby`/`$select`.
    pub fn parse(pairs: &[(String, String)]) -> OagwResult<Self> {
        let mut query = Self {
            top: DEFAULT_TOP,
            ..Self::default()
        };
        for (key, value) in pairs {
            match key.as_str() {
                "$top" => {
                    let parsed = usize::try_from(parse_number(key, value)?).unwrap_or(usize::MAX);
                    if parsed == 0 {
                        return Err(OagwError::validation("$top must be at least 1")
                            .with_extension(|ext| ext.invalid_value = Some(value.to_owned())));
                    }
                    if parsed > MAX_TOP {
                        return Err(OagwError::validation(format!(
                            "$top must not exceed {MAX_TOP}"
                        )));
                    }
                    query.top = parsed;
                }
                "$skip" => {
                    query.skip = usize::try_from(parse_number(key, value)?).unwrap_or(usize::MAX);
                }
                "$filter" => query.filter = parse_filter(key, value)?,
                "$orderby" => query.orderby = parse_order_clause(value, key)?,
                "$select" => {
                    let fields = parse_select(value).map_err(|error| {
                        OagwError::validation(format!("$select is invalid: {error}"))
                    })?;
                    query.select = Some(fields);
                }
                _ => {}
            }
        }
        Ok(query)
    }

    /// Whether a filter narrows the result set.
    #[must_use]
    pub const fn has_filter(&self) -> bool {
        self.filter.is_some()
    }
}

fn parse_number(key: &str, raw: &str) -> OagwResult<u64> {
    raw.trim().parse::<u64>().map_err(|_| {
        OagwError::validation(format!("{key} must be a non-negative integer"))
            .with_extension(|ext| ext.invalid_value = Some(raw.to_owned()))
    })
}

/// Parse `<field> eq <value>` (the operator OAGW's contract documents).
fn parse_filter(key: &str, raw: &str) -> OagwResult<Option<(String, String)>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let [field, operator, value] = tokens.as_slice() else {
        return Err(OagwError::validation(format!(
            "{key} must be '<field> eq <value>'"
        )));
    };
    if !operator.eq_ignore_ascii_case("eq") {
        return Err(OagwError::validation(format!(
            "{key} only supports the 'eq' operator"
        )));
    }
    let value = unquote(value);
    Ok(Some((field.to_ascii_lowercase(), value)))
}

/// Strip `OData` single quotes (and tolerate double quotes) from a literal.
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    for quote in ['\'', '"'] {
        if trimmed.starts_with(quote) && trimmed.ends_with(quote) && trimmed.len() >= 2 {
            return trimmed[1..trimmed.len() - 1].to_owned();
        }
    }
    trimmed.to_owned()
}

fn parse_order_clause(raw: &str, key: &str) -> OagwResult<Vec<(String, bool)>> {
    let parsed = parse_orderby(raw)
        .map_err(|error| OagwError::validation(format!("{key} is invalid: {error}")))?;
    Ok(parsed
        .0
        .into_iter()
        .map(|order| (order.field.to_ascii_lowercase(), order.dir == SortDir::Desc))
        .collect())
}

/// Filter, order, offset and project `items`.
#[must_use]
pub fn apply_list_query(items: Vec<Value>, query: &ListQuery) -> Vec<Value> {
    let filtered = match &query.filter {
        Some((field, expected)) => items
            .into_iter()
            .filter(|item| matches_filter(item, field, expected))
            .collect(),
        None => items,
    };
    let ordered = order_items(filtered, &query.orderby);
    let page: Vec<Value> = ordered
        .into_iter()
        .skip(query.skip)
        .take(query.top)
        .collect();
    let Some(fields) = &query.select else {
        return page;
    };
    page.iter()
        .map(|item| project_json(item, &selected_set(fields)))
        .collect()
}

fn selected_set(fields: &[String]) -> std::collections::HashSet<String> {
    fields.iter().cloned().collect()
}

/// `eq` comparison against the JSON rendering of `field`.
fn matches_filter(item: &Value, field: &str, expected: &str) -> bool {
    item.get(field)
        .is_some_and(|actual| render(actual).eq_ignore_ascii_case(expected))
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// Stable multi-key sort; records missing a key keep their relative order.
fn order_items(mut items: Vec<Value>, orderby: &[(String, bool)]) -> Vec<Value> {
    if orderby.is_empty() {
        return items;
    }
    items.sort_by(|left, right| {
        for (field, descending) in orderby {
            let ordering = compare_fields(left.get(field), right.get(field));
            let ordering = if *descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering.is_ne() {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
    items
}

fn compare_fields(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_values(left, right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Variant-aware comparison: numbers sort numerically, everything else falls
/// back to its JSON rendering.
///
/// A string-rendered comparison would order `443` before `80`, so numeric
/// members (`port`, `rate`) are compared as numbers.
fn compare_values(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => compare_numbers(left, right),
        (Value::Number(_), _) => std::cmp::Ordering::Less,
        (_, Value::Number(_)) => std::cmp::Ordering::Greater,
        _ => render(left).cmp(&render(right)),
    }
}

/// Numeric comparison: integers compare exactly, mixed or non-integer values
/// fall back to `f64` (unrepresentable values compare equal).
fn compare_numbers(left: &serde_json::Number, right: &serde_json::Number) -> std::cmp::Ordering {
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        return left.cmp(&right);
    }
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return left.cmp(&right);
    }
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left.total_cmp(&right),
        _ => std::cmp::Ordering::Equal,
    }
}

/// Wrap projected items in the platform page envelope.
#[must_use]
pub fn to_page(items: Vec<Value>, top: usize) -> Page<Value> {
    Page::new(
        items,
        toolkit::PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: u64::try_from(top).unwrap_or(u64::MAX),
        },
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DEFAULT_TOP, ListQuery, MAX_TOP, apply_list_query, parse_filter, to_page};

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_to_fifty_items() {
        let query = ListQuery::parse(&[]).ok();
        assert_eq!(query.as_ref().map(|query| query.top), Some(DEFAULT_TOP));
        assert_eq!(query.map(|query| query.skip), Some(0));
    }

    #[test]
    fn parses_top_and_skip() {
        let query = ListQuery::parse(&pairs(&[("$top", "7"), ("$skip", "3")]));
        assert_eq!(
            query.ok().map(|query| (query.top, query.skip)),
            Some((7, 3))
        );
    }

    #[test]
    fn clamps_top_at_the_documented_maximum() {
        let query = ListQuery::parse(&pairs(&[("$top", "500")]));
        assert!(query.is_err());
        let ok = ListQuery::parse(&pairs(&[("$top", &MAX_TOP.to_string())]));
        assert_eq!(ok.ok().map(|query| query.top), Some(MAX_TOP));
    }

    #[test]
    fn rejects_non_numeric_paging() {
        assert!(ListQuery::parse(&pairs(&[("$top", "-1")])).is_err());
        assert!(ListQuery::parse(&pairs(&[("$skip", "many")])).is_err());
    }

    #[test]
    fn parses_an_eq_filter_with_quotes() {
        let filter = parse_filter("$filter", "alias eq 'api.openai.com'")
            .ok()
            .flatten();
        assert_eq!(
            filter,
            Some(("alias".to_owned(), "api.openai.com".to_owned()))
        );
        let bare = parse_filter("$filter", "enabled eq true").ok().flatten();
        assert_eq!(bare, Some(("enabled".to_owned(), "true".to_owned())));
    }

    #[test]
    fn rejects_non_eq_operators() {
        assert!(parse_filter("$filter", "alias ne 'x'").is_err());
        assert!(parse_filter("$filter", "alias").is_err());
    }

    #[test]
    fn filters_orders_and_paginates() {
        let items = vec![
            json!({"alias": "b.vendor.com", "enabled": true}),
            json!({"alias": "a.vendor.com", "enabled": false}),
            json!({"alias": "c.vendor.com", "enabled": true}),
        ];
        let query = ListQuery::parse(&pairs(&[
            ("$filter", "enabled eq true"),
            ("$orderby", "alias"),
        ]))
        .ok();
        let projected = query.map(|query| apply_list_query(items, &query));
        assert_eq!(
            projected,
            Some(vec![
                json!({"alias": "b.vendor.com", "enabled": true}),
                json!({"alias": "c.vendor.com", "enabled": true}),
            ])
        );
    }

    #[test]
    fn orders_descending() {
        let items = vec![json!({"alias": "a"}), json!({"alias": "b"})];
        let query = ListQuery::parse(&pairs(&[("$orderby", "alias desc")])).ok();
        let ordered = query.map(|query| apply_list_query(items, &query));
        assert_eq!(
            ordered.map(|items| items[0]["alias"].clone()),
            Some(json!("b"))
        );
    }

    #[test]
    fn numeric_members_sort_numerically() {
        // A string rendering would order `"443"` before `"80"`.
        let items = vec![json!({"port": 443}), json!({"port": 80})];
        let query = ListQuery::parse(&pairs(&[("$orderby", "port")])).ok();
        let ordered = query.map(|query| apply_list_query(items, &query));
        let ports: Vec<i64> = ordered
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item["port"].as_i64())
            .collect();
        assert_eq!(ports, vec![80, 443]);
    }

    #[test]
    fn numbers_sort_before_other_members() {
        let items = vec![json!({"rate": "x"}), json!({"rate": 1})];
        let query = ListQuery::parse(&pairs(&[("$orderby", "rate")])).ok();
        let ordered = query.map(|query| apply_list_query(items, &query));
        assert_eq!(
            ordered.map(|items| items[0]["rate"].clone()),
            Some(json!(1))
        );
    }

    #[test]
    fn selects_a_subset_of_fields() {
        let items = vec![json!({"id": "1", "alias": "a", "enabled": true})];
        let query = ListQuery::parse(&pairs(&[("$select", "id,alias")])).ok();
        let projected = query.map(|query| apply_list_query(items, &query));
        assert_eq!(projected, Some(vec![json!({"id": "1", "alias": "a"})]));
    }

    #[test]
    fn skips_beyond_the_collection() {
        let items = vec![json!({"alias": "a"})];
        let query = ListQuery::parse(&pairs(&[("$skip", "5")])).ok();
        let projected = query.map(|query| apply_list_query(items, &query));
        assert_eq!(projected, Some(Vec::new()));
    }

    #[test]
    fn page_envelope_carries_the_effective_limit() {
        let page = to_page(Vec::new(), 25);
        assert_eq!(page.page_info.limit, 25);
        assert!(page.items.is_empty());
    }
}
