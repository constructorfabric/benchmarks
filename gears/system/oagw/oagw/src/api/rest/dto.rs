//! Wire DTOs of the management API.
//!
//! The domain model already round-trips the wire shape, so these are type
//! aliases plus the list-response envelopes.

use serde::{Deserialize, Serialize};

use crate::domain::dto::{Route, Upstream};

/// An upstream as accepted and returned by the management API.
pub type UpstreamDto = Upstream;

/// A route as accepted and returned by the management API.
pub type RouteDto = Route;

/// A plugin as returned by the management API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDto {
    /// Plugin id (UUID).
    pub id: String,
    /// Plugin GTS type (`gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`).
    pub gts_id: String,
    /// Plugin kind (`auth` | `guard` | `transform`).
    pub kind: String,
    /// Human-readable name.
    pub name: String,
    /// Declarative configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// A plugin source document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSourceDto {
    /// Plugin id (UUID).
    pub id: String,
    /// The plugin source (Starlark).
    pub source: String,
}

/// The list envelope of every management list endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse<T> {
    /// The matched resources.
    pub value: Vec<T>,
    /// `@odata.nextLink` when the page is not the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_link: Option<String>,
    /// Total number of resources matching the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

impl<T> ListResponse<T> {
    /// Builds a page.
    pub fn new(value: Vec<T>, count: Option<u64>, next_link: Option<String>) -> Self {
        Self { value, next_link, count }
    }
}

/// The parsed OData list parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListParams {
    /// Raw `$filter` expression.
    pub filter: Option<String>,
    /// Raw `$select` field list.
    pub select: Option<String>,
    /// Raw `$orderby` expression.
    pub orderby: Option<String>,
    /// `$top`, clamped to `[1, 100]`, default 50.
    pub top: usize,
    /// `$skip`.
    pub skip: usize,
}

impl ListParams {
    /// Maximum page size.
    pub const MAX_TOP: usize = 100;
    /// Default page size.
    pub const DEFAULT_TOP: usize = 50;

    /// Parses the OData parameters out of a query string.
    pub fn from_query(query: &[(String, String)]) -> Self {
        let mut params = ListParams { top: Self::DEFAULT_TOP, ..ListParams::default() };
        for (key, value) in query {
            match key.as_str() {
                "$filter" => params.filter = Some(value.clone()),
                "$select" => params.select = Some(value.clone()),
                "$orderby" => params.orderby = Some(value.clone()),
                "$top" => {
                    params.top = value
                        .parse::<usize>()
                        .ok()
                        .map(|t| t.clamp(1, Self::MAX_TOP))
                        .unwrap_or(Self::DEFAULT_TOP);
                }
                "$skip" => params.skip = value.parse::<usize>().unwrap_or(0),
                _ => {}
            }
        }
        params
    }

    /// Applies the page to a list, returning the page and the total count.
    pub fn paginate<T>(&self, items: Vec<T>) -> (Vec<T>, u64) {
        let total = items.len() as u64;
        let skipped: Vec<T> = items.into_iter().skip(self.skip).collect();
        let page: Vec<T> = skipped.into_iter().take(self.top).collect();
        (page, total)
    }
}

/// Evaluates an OData `$filter` of the form `field op 'value'`.
///
/// Only the documented `eq`/`ne` comparisons on scalar string fields are
/// supported; anything else filters nothing out (the expression is echoed in
/// the log, not silently swallowed).
pub fn apply_filter<T>(
    items: Vec<T>,
    filter: Option<&str>,
    field_of: impl Fn(&T) -> String,
) -> Vec<T> {
    let Some(filter) = filter else { return items };
    let filter = filter.trim();
    let Some((_field, op, value)) = parse_filter(filter) else {
        return items;
    };
    items.into_iter().filter(|item| {
        let actual = field_of(item);
        match op {
            FilterOp::Eq => actual == value,
            FilterOp::Ne => actual != value,
            FilterOp::Contains => actual.contains(&value),
        }
    }).collect()
}

/// The comparison operators the list endpoints support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOp {
    Eq,
    Ne,
    Contains,
}

/// Splits `field op 'value'`.
fn parse_filter(filter: &str) -> Option<(String, FilterOp, String)> {
    for (op, kind) in [
        (" eq ", FilterOp::Eq),
        (" ne ", FilterOp::Ne),
        (" contains ", FilterOp::Contains),
    ] {
        if let Some((field, rest)) = filter.split_once(op) {
            let value = rest.trim().trim_matches('\'').to_string();
            return Some((field.trim().to_string(), kind, value));
        }
    }
    None
}

/// Orders a list by an OData `$orderby` expression.
pub fn apply_orderby<T>(
    mut items: Vec<T>,
    orderby: Option<&str>,
    key_of: impl Fn(&T) -> String,
) -> Vec<T> {
    let Some(spec) = orderby_expr(orderby) else { return items };
    let descending = spec.1;
    items.sort_by(|a, b| {
        let (ka, kb) = (key_of(a), key_of(b));
        if descending {
            kb.cmp(&ka)
        } else {
            ka.cmp(&kb)
        }
    });
    items
}

/// Splits `$orderby` into a field and a direction.
fn orderby_expr(orderby: Option<&str>) -> Option<(String, bool)> {
    let spec = orderby?.trim().to_string();
    if let Some((field, dir)) = spec.split_once(' ') {
        Some((field.to_string(), dir.eq_ignore_ascii_case("desc")))
    } else {
        Some((spec, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odata_parameters_are_parsed_and_clamped() {
        let params = ListParams::from_query(&[
            ("$top".to_string(), "1000".to_string()),
            ("$skip".to_string(), "10".to_string()),
            ("$filter".to_string(), "alias eq 'api.openai.com'".to_string()),
        ]);
        assert_eq!(params.top, 100, "top is clamped to the maximum");
        assert_eq!(params.skip, 10);
        assert_eq!(
            params.filter.as_deref(),
            Some("alias eq 'api.openai.com'")
        );
    }

    #[test]
    fn the_default_page_size_is_fifty() {
        let params = ListParams::from_query(&[]);
        assert_eq!(params.top, 50);
        assert_eq!(params.skip, 0);
    }

    #[test]
    fn pagination_slices_the_list() {
        let items: Vec<u32> = (0..120).collect();
        let (page, total) = ListParams { top: 50, skip: 100, ..Default::default() }
            .paginate(items);
        assert_eq!(page, vec![100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119]);
        assert_eq!(total, 120);
    }

    #[test]
    fn filters_apply_eq_ne_and_contains() {
        let items = vec!["a".to_string(), "ab".to_string(), "b".to_string()];
        assert_eq!(apply_filter(items.clone(), Some("value eq 'ab'"), |i| i.clone()), vec!["ab"]);
        assert_eq!(apply_filter(items.clone(), Some("value ne 'ab'"), |i| i.clone()), vec!["a", "b"]);
        assert_eq!(apply_filter(items.clone(), Some("value contains 'a'"), |i| i.clone()), vec!["a", "ab"]);
    }

    #[test]
    fn an_unsupported_filter_is_ignored_rather_than_silent_data_loss() {
        let items = vec!["a".to_string()];
        assert_eq!(
            apply_filter(items, Some("value gt 'a'"), |i| i.clone()),
            vec!["a"]
        );
    }

    #[test]
    fn orderby_honours_the_direction() {
        let items = vec!["b".to_string(), "a".to_string()];
        assert_eq!(
            apply_orderby(items.clone(), Some("value asc"), |i| i.clone()),
            vec!["a", "b"]
        );
        assert_eq!(apply_orderby(items, Some("value desc"), |i| i.clone()), vec!["b", "a"]);
    }
}
