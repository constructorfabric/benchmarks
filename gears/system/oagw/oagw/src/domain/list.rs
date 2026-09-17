//! OData-style list-query handling for management endpoints (DESIGN §3.3
//! "List Query Parameters").
//!
//! Implements `$filter` (equality only), `$select` (validated, projected by
//! the REST layer), `$orderby` (`field [asc|desc]`), `$top` (default 50,
//! max 100) and `$skip` against a typed item source. Unsupported operators or
//! unknown fields are rejected with a validation error instead of being
//! silently ignored.

use crate::domain::dto::ListParams;
use crate::domain::error::{OagwError, OagwResult};

/// Cap enforced for `$top`.
pub const DEFAULT_TOP: usize = 50;
/// Maximum value accepted for `$top`.
pub const MAX_TOP: usize = 100;

/// A resource exposing listable fields.
pub trait ListItem {
    /// Sort/select keys: subset of what `$orderby`/`$select` may reference,
    /// e.g. `["id", "alias", "enabled"]`.
    fn list_fields() -> &'static [&'static str];

    /// String rendering of `field` for equality comparisons and sorting.
    fn field_value(&self, field: &str) -> Option<String>;
}

/// A parsed, validated list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// `(field, value)` equality filter.
    pub filter: Option<(String, String)>,
    /// `(field, ascending)` sort key.
    pub order_by: Option<(String, bool)>,
    /// Number of results to return (default 50).
    pub top: usize,
    /// Offset for pagination.
    pub skip: usize,
    /// `$select` field names (validated; projection is the caller's job).
    pub select: Vec<String>,
}

/// Parse and validate the raw list parameters against the allowed field set.
///
/// # Errors
///
/// Returns a validation error for unknown fields, malformed or unsupported
/// `$filter`/`$orderby` expressions, or `$top` outside `1..=100`.
pub fn parse_query(params: &ListParams, allowed: &[&str]) -> OagwResult<ListQuery> {
    let check_field = |name: &str, source: &str| -> OagwResult<()> {
        if !allowed.iter().any(|f| *f == name) {
            return Err(OagwError::validation(format!(
                "unknown {source} field {name:?}; supported fields: {}",
                allowed.join(", ")
            )));
        }
        Ok(())
    };

    let filter = match params.filter.as_deref() {
        None => None,
        Some(expr) => {
            let trimmed = expr.trim();
            let upper = trimmed.to_ascii_uppercase();
            let pos = upper.find(" EQ ").ok_or_else(|| {
                OagwError::validation(format!(
                    "unsupported $filter expression {expr:?}; only `field eq value` is supported"
                ))
            })?;
            let field = trimmed[..pos].trim();
            let raw_value = trimmed[pos + 4..].trim();
            check_field(field, "$filter")?;
            let value = raw_value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .unwrap_or(raw_value);
            if value.is_empty() {
                return Err(OagwError::validation("empty $filter value".to_owned()));
            }
            Some((field.to_owned(), value.to_owned()))
        }
    };

    let order_by = match params.orderby.as_deref() {
        None => None,
        Some(expr) => {
            let trimmed = expr.trim();
            let mut parts = trimmed.split_whitespace();
            let field = parts
                .next()
                .ok_or_else(|| OagwError::validation("empty $orderby expression".to_owned()))?;
            check_field(field, "$orderby")?;
            let dir = parts.next().unwrap_or("asc");
            let ascending = match dir.to_ascii_lowercase().as_str() {
                "asc" => true,
                "desc" => false,
                other => {
                    return Err(OagwError::validation(format!(
                        "invalid $orderby direction {other:?}; use asc or desc"
                    )));
                }
            };
            if parts.next().is_some() {
                return Err(OagwError::validation(format!(
                    "invalid $orderby expression {expr:?}"
                )));
            }
            Some((field.to_owned(), ascending))
        }
    };

    let top = match params.top {
        None => DEFAULT_TOP,
        Some(t) => {
            if t == 0 || t as usize > MAX_TOP {
                return Err(OagwError::validation(format!(
                    "$top must be between 1 and {MAX_TOP}"
                )));
            }
            t as usize
        }
    };

    let select = match params.select.as_deref() {
        None => Vec::new(),
        Some(list) => {
            let fields: Vec<&str> = list
                .split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .collect();
            if fields.is_empty() {
                return Err(OagwError::validation("empty $select list".to_owned()));
            }
            for f in &fields {
                check_field(f, "$select")?;
            }
            fields.into_iter().map(str::to_owned).collect()
        }
    };

    Ok(ListQuery {
        filter,
        order_by,
        top,
        skip: params.skip.unwrap_or(0) as usize,
        select,
    })
}

/// Apply a parsed query to an item collection.
///
/// Returns `(paged_items, total_before_paging)`.
pub fn apply<T: ListItem>(query: &ListQuery, items: Vec<T>) -> OagwResult<(Vec<T>, usize)> {
    let mut items = items;
    if let Some((field, value)) = &query.filter {
        items.retain(|item| item.field_value(field).as_deref() == Some(value.as_str()));
    }
    if let Some((field, ascending)) = &query.order_by {
        items.sort_by(|a, b| {
            let av = a.field_value(field).unwrap_or_default();
            let bv = b.field_value(field).unwrap_or_default();
            if *ascending { av.cmp(&bv) } else { bv.cmp(&av) }
        });
    }
    let total = items.len();
    let skipped = items.into_iter().skip(query.skip).take(query.top).collect();
    Ok((skipped, total))
}

/// Convenience: apply a raw parameter set to an item collection.
pub fn apply_params<T: ListItem>(
    params: &ListParams,
    allowed: &[&str],
    items: Vec<T>,
) -> OagwResult<(Vec<T>, usize)> {
    let query = parse_query(params, allowed)?;
    apply(&query, items)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    struct Row {
        id: u32,
        alias: String,
        enabled: bool,
    }

    impl ListItem for Row {
        fn list_fields() -> &'static [&'static str] {
            &["id", "alias", "enabled"]
        }

        fn field_value(&self, field: &str) -> Option<String> {
            match field {
                "id" => Some(self.id.to_string()),
                "alias" => Some(self.alias.clone()),
                "enabled" => Some(self.enabled.to_string()),
                _ => None,
            }
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            Row {
                id: 1,
                alias: "beta.example".into(),
                enabled: true,
            },
            Row {
                id: 2,
                alias: "alpha.example".into(),
                enabled: true,
            },
            Row {
                id: 3,
                alias: "beta.example".into(),
                enabled: false,
            },
        ]
    }

    #[test]
    fn filter_select_order_page() {
        let params = ListParams {
            filter: Some("alias eq 'beta.example'".to_owned()),
            select: Some("id,alias".to_owned()),
            orderby: Some("id desc".to_owned()),
            top: Some(10),
            skip: None,
        };
        let query = parse_query(&params, Row::list_fields()).unwrap();
        assert_eq!(
            query.filter,
            Some(("alias".to_owned(), "beta.example".to_owned()))
        );
        let (items, total) = apply(&query, rows()).unwrap();
        assert_eq!(total, 2);
        assert_eq!(query.select, vec!["id".to_owned(), "alias".to_owned()]);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn invalid_operator_and_field_rejected() {
        let params = ListParams {
            filter: Some("alias ne 'x'".to_owned()),
            ..Default::default()
        };
        assert!(parse_query(&params, Row::list_fields()).is_err());
        let params = ListParams {
            orderby: Some("bogus asc".to_owned()),
            ..Default::default()
        };
        assert!(parse_query(&params, Row::list_fields()).is_err());
        let params = ListParams {
            top: Some(1000),
            ..Default::default()
        };
        assert!(parse_query(&params, Row::list_fields()).is_err());
    }

    #[test]
    fn top_and_skip_pagination() {
        let params = ListParams {
            top: Some(2),
            skip: Some(1),
            ..Default::default()
        };
        let query = parse_query(&params, Row::list_fields()).unwrap();
        let (items, total) = apply(&query, rows()).unwrap();
        assert_eq!(total, 3);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 2);
    }
}
