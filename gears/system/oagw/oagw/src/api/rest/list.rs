//! OData-flavoured list parameters of the management API.
//!
//! Collection endpoints accept the subset of the OData query grammar the
//! platform uses elsewhere: `$top`, `$skip`, `$filter` (single `field op
//! value` clause), `$orderby` and `$select`. Anything `$`-prefixed that is not
//! part of the grammar, or a malformed value, is rejected so the handler can
//! answer `400` instead of silently ignoring a typo.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest accepted `$top`.
pub const MAX_PAGE_SIZE: usize = 100;
/// Page size used when `$top` is absent.
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Query parameters accepted by every collection endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListParams {
    /// `$filter` clause, at most one.
    pub filter: Option<FilterClause>,
    /// `$orderby` clause.
    pub orderby: Option<OrderClause>,
    /// `$select` field list.
    pub select: Option<Vec<String>>,
    /// `$top`.
    pub top: Option<usize>,
    /// `$skip`, `0` when absent.
    pub skip: usize,
}

/// A single `field op value` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterClause {
    /// Field to compare.
    pub field: String,
    /// Comparison operator.
    pub op: FilterOp,
    /// Literal to compare against.
    pub value: String,
}

/// Supported `$filter` operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// Equality.
    Eq,
    /// Inequality.
    Ne,
}

/// An `$orderby` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    /// Field to sort by.
    pub field: String,
    /// Whether the sort is descending.
    pub descending: bool,
}

/// Page envelope of a collection response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    /// Items of this page.
    pub items: Vec<T>,
    /// Paging metadata.
    pub page_info: PageInfo,
}

/// Paging metadata, mirroring `toolkit_odata::PageInfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInfo {
    /// Cursor of the next page; always `null`, collections are fully paged.
    pub next_cursor: Option<String>,
    /// Cursor of the previous page; always `null`.
    pub prev_cursor: Option<String>,
    /// Page size that was applied.
    pub limit: u64,
}

impl ListParams {
    /// Parses a decoded query string.
    ///
    /// # Errors
    /// Returns a client-facing message when an unknown `$`-prefixed key or a
    /// malformed value is supplied.
    pub fn parse(query: &HashMap<String, String>) -> Result<Self, String> {
        let mut params = Self::default();
        for (key, raw) in query {
            match key.as_str() {
                "$filter" => params.filter = Some(parse_filter(raw)?),
                "$orderby" => params.orderby = Some(parse_order(raw)?),
                "$select" => params.select = Some(parse_select(raw)?),
                "$top" => params.top = Some(parse_count(raw, "top")?),
                "$skip" => params.skip = parse_count(raw, "skip")?,
                other if other.starts_with('$') => {
                    return Err(format!("unknown list parameter {other:?}"));
                }
                // Non-OData keys are forwarded untouched to the next hop.
                _ => {}
            }
        }
        Ok(params)
    }

    /// Applies filtering, ordering, paging and projection to `items`.
    #[must_use]
    pub fn apply(&self, items: Vec<Value>) -> Vec<Value> {
        let filtered: Vec<Value> = match &self.filter {
            Some(clause) => items
                .into_iter()
                .filter(|item| matches_clause(item, clause))
                .collect(),
            None => items,
        };
        let sorted = match &self.orderby {
            Some(clause) => {
                let mut sorted = filtered;
                sorted.sort_by(|a, b| {
                    let ordering = compare(
                        a.get(&clause.field).unwrap_or(&Value::Null),
                        b.get(&clause.field).unwrap_or(&Value::Null),
                    );
                    if clause.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                });
                sorted
            }
            None => filtered,
        };
        sorted
            .into_iter()
            .skip(self.skip)
            .take(self.top.unwrap_or(DEFAULT_PAGE_SIZE))
            .map(|item| project(&item, self.select.as_deref()))
            .collect()
    }

    /// Builds the page envelope of `items`.
    #[must_use]
    pub fn page<T>(&self, items: Vec<T>) -> Page<T> {
        let limit = self.top.unwrap_or(DEFAULT_PAGE_SIZE);
        Page {
            items,
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: limit as u64,
            },
        }
    }
}

fn parse_count(raw: &str, name: &str) -> Result<usize, String> {
    let parsed = raw
        .parse::<usize>()
        .map_err(|_| format!("${name} must be a non-negative integer, got {raw:?}"))?;
    if name == "top" && parsed > MAX_PAGE_SIZE {
        return Err(format!(
            "$top must not exceed {MAX_PAGE_SIZE}, got {parsed}"
        ));
    }
    Ok(parsed)
}

fn parse_select(raw: &str) -> Result<Vec<String>, String> {
    let fields: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(str::to_owned)
        .collect();
    if fields.is_empty() {
        return Err("$select must name at least one field".to_owned());
    }
    Ok(fields)
}

fn parse_order(raw: &str) -> Result<OrderClause, String> {
    let raw = raw.trim();
    let (field, direction) = raw
        .rsplit_once(char::is_whitespace)
        .map_or((raw, "asc"), |(field, rest)| (field.trim(), rest.trim()));
    let descending = match direction {
        "asc" => false,
        "desc" => true,
        _ => {
            return Err(format!(
                "$orderby direction must be asc or desc, got {raw:?}"
            ));
        }
    };
    if field.is_empty() {
        return Err("$orderby must name a field".to_owned());
    }
    Ok(OrderClause {
        field: field.to_owned(),
        descending,
    })
}

fn parse_filter(raw: &str) -> Result<FilterClause, String> {
    let raw = raw.trim();
    if raw.contains(" and ") || raw.contains(" or ") || raw.contains('(') {
        return Err(format!(
            "only a single filter clause is supported, got {raw:?}"
        ));
    }
    let (field, rest) = raw
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("$filter must be `field op value`, got {raw:?}"))?;
    let (op, value) = rest
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("$filter must be `field op value`, got {raw:?}"))?;
    let op = match op.trim() {
        "eq" => FilterOp::Eq,
        "ne" => FilterOp::Ne,
        other => return Err(format!("$filter operator must be eq or ne, got {other:?}")),
    };
    let value = unquote(value.trim());
    if value.is_empty() {
        return Err(format!("$filter value must not be empty, got {raw:?}"));
    }
    Ok(FilterClause {
        field: field.trim().to_owned(),
        op,
        value,
    })
}

/// Strips the single quotes of an OData string literal.
fn unquote(value: &str) -> String {
    value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(value)
        .to_owned()
}

fn matches_clause(item: &Value, clause: &FilterClause) -> bool {
    let Some(actual) = item.get(&clause.field) else {
        return clause.op == FilterOp::Ne;
    };
    let equal = match actual {
        // `tags eq 'chat'` selects every item whose tag list carries the tag.
        Value::Array(entries) => entries
            .iter()
            .any(|entry| scalar_text(entry).as_deref() == Some(clause.value.as_str())),
        other => {
            scalar_text(other).as_deref() == Some(clause.value.as_str())
                || other == &Value::String(clause.value.clone())
        }
    };
    match clause.op {
        FilterOp::Eq => equal,
        FilterOp::Ne => !equal,
    }
}

/// Renders a JSON scalar so an unquoted literal can be compared against it.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::Bool(inner) => Some(inner.to_string()),
        Value::Number(inner) => Some(inner.to_string()),
        Value::String(inner) => Some(inner.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Total order over JSON scalars: `null < false < true < numbers < strings`.
fn compare(a: &Value, b: &Value) -> Ordering {
    let rank = |value: &Value| match value {
        Value::Null => 0_u8,
        Value::Bool(false) => 1,
        Value::Bool(true) => 2,
        Value::Number(_) => 3,
        Value::String(_) => 4,
        Value::Array(_) | Value::Object(_) => 5,
    };
    rank(a).cmp(&rank(b)).then_with(|| match (a, b) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .unwrap_or_default()
            .partial_cmp(&b.as_f64().unwrap_or_default())
            .unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => Ordering::Equal,
    })
}

/// Keeps only the top level fields named by `select`.
#[must_use]
pub fn project(item: &Value, select: Option<&[String]>) -> Value {
    let Some(fields) = select else {
        return item.clone();
    };
    let mut projected = serde_json::Map::new();
    for field in fields {
        if let Some(value) = item.get(field) {
            projected.insert(field.clone(), value.clone());
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn items() -> Vec<Value> {
        vec![
            json!({ "id": "a", "alias": "zeta.example.com", "enabled": true }),
            json!({ "id": "b", "alias": "alpha.example.com", "enabled": false }),
            json!({ "id": "c", "alias": "mid.example.com", "enabled": true }),
        ]
    }

    #[test]
    fn empty_parameters_yield_the_default_page() {
        let params = ListParams::parse(&query(&[])).expect("parse");
        assert_eq!(params, ListParams::default());
        let page = params.page(params.apply(items()));
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.page_info.limit, DEFAULT_PAGE_SIZE as u64);
        assert!(page.page_info.next_cursor.is_none());
    }

    #[test]
    fn filter_matches_strings_and_scalars() {
        let params =
            ListParams::parse(&query(&[("$filter", "alias eq 'mid.example.com'")])).expect("parse");
        let filtered = params.apply(items());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["id"], "c");

        let params = ListParams::parse(&query(&[("$filter", "enabled eq false")])).expect("parse");
        let filtered = params.apply(items());
        assert_eq!(filtered[0]["id"], "b");

        let params = ListParams::parse(&query(&[("$filter", "enabled ne true")])).expect("parse");
        assert_eq!(params.apply(items())[0]["id"], "b");
    }

    #[test]
    fn missing_filter_fields_never_match_equality() {
        let params = ListParams::parse(&query(&[("$filter", "alias eq 'ghost'")])).expect("parse");
        let ghost = vec![json!({ "id": "d" })];
        assert!(params.apply(ghost).is_empty());
    }

    #[test]
    fn array_fields_match_when_they_contain_the_value() {
        let params = ListParams::parse(&query(&[("$filter", "tags eq 'chat'")])).expect("parse");
        let tagged = vec![
            json!({ "id": "a", "tags": ["chat", "llm"] }),
            json!({ "id": "b", "tags": ["other"] }),
        ];
        let matched = params.apply(tagged);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0]["id"], "a");
    }

    #[test]
    fn orderby_sorts_ascending_and_descending() {
        let params = ListParams::parse(&query(&[("$orderby", "alias asc")])).expect("parse");
        let sorted = params.apply(items());
        assert_eq!(sorted[0]["id"], "b");

        let params = ListParams::parse(&query(&[("$orderby", "alias desc")])).expect("parse");
        let sorted = params.apply(items());
        assert_eq!(sorted[0]["id"], "a");
    }

    #[test]
    fn paging_slices_the_collection() {
        let params = ListParams::parse(&query(&[("$skip", "1"), ("$top", "1")])).expect("parse");
        let page = params.page(params.apply(items()));
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.page_info.limit, 1);
        assert_eq!(page.items[0]["id"], "b");
    }

    #[test]
    fn select_projects_the_named_fields() {
        let params = ListParams::parse(&query(&[("$select", "alias")])).expect("parse");
        let projected = params.apply(items());
        assert_eq!(projected[0], json!({ "alias": "zeta.example.com" }));
        assert!(projected[0].get("id").is_none());
    }

    #[test]
    fn unknown_and_malformed_parameters_are_rejected() {
        for (key, value) in [
            ("$unknown", "1"),
            ("$top", "many"),
            ("$top", "-1"),
            ("$skip", "nope"),
            ("$filter", "alias"),
            ("$filter", "alias eq"),
            ("$filter", "alias gt 'x'"),
            ("$filter", "alias eq 'a' and alias eq 'b'"),
            ("$orderby", "alias sideways"),
            ("$orderby", ""),
            ("$select", "  "),
        ] {
            let error = ListParams::parse(&query(&[(key, value)]))
                .expect_err("malformed parameters must be rejected");
            assert!(!error.is_empty(), "{key}={value}");
        }
    }

    #[test]
    fn top_is_capped_at_the_maximum_page_size() {
        let error = ListParams::parse(&query(&[("$top", "101")])).expect_err("over the cap");
        assert!(error.contains("must not exceed"), "{error}");
        let params = ListParams::parse(&query(&[("$top", "100")])).expect("at the cap");
        assert_eq!(params.top, Some(MAX_PAGE_SIZE));
    }

    #[test]
    fn non_odata_keys_are_ignored() {
        let params = ListParams::parse(&query(&[("traceparent", "00-abc-00-01"), ("$top", "2")]))
            .expect("parse");
        assert_eq!(params.top, Some(2));
    }
}
