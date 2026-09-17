//! Wire DTOs for the management API list endpoints.
//!
//! `toolkit-odata` is not an `oagw` dependency, so the OData query surface is
//! accepted here on the documented subset: `$filter` (equality only),
//! `$select`, `$orderby` (single field, optional `desc`), `$top` (≤ 100) and
//! `$skip`.

use serde::Deserialize;

use crate::domain::error::DomainError;
use crate::domain::services::ListQuery;

/// OData-style list query parameters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParams {
    /// `field eq 'value'` equality filters.
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    /// Fields to return.
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    /// Page size (default 50, max 100).
    #[serde(rename = "$top", default)]
    pub top: Option<usize>,
    /// Offset.
    #[serde(rename = "$skip", default)]
    pub skip: Option<usize>,
    /// `field [desc]`.
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
}

impl ListParams {
    /// Converts the query parameters into the domain [`ListQuery`].
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] for an unparseable `$filter`/`$orderby`.
    pub fn to_list_query(&self) -> Result<ListQuery, DomainError> {
        let mut query = ListQuery {
            filters: Vec::new(),
            order_by: None,
            top: self.top,
            skip: self.skip,
        };
        if let Some(raw) = self.filter.as_deref() {
            query.filters = parse_filter(raw)?;
        }
        if let Some(raw) = self.orderby.as_deref() {
            query.order_by = parse_orderby(raw)?;
        }
        Ok(query)
    }

    /// Selected field names (`$select`), when supplied.
    #[must_use]
    pub fn selected_fields(&self) -> Vec<String> {
        self.select
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Parses `a eq 'x', b eq 'y'` into `(field, value)` pairs.
fn parse_filter(raw: &str) -> Result<Vec<(String, String)>, DomainError> {
    let mut filters = Vec::new();
    for clause in raw.split(',') {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        let Some((field, value)) = clause.split_once(" eq ") else {
            return Err(DomainError::Validation(format!(
                "$filter '{clause}' is not supported: only `<field> eq '<value>'` is accepted"
            )));
        };
        let field = field.trim();
        let value = value.trim().trim_matches('\'');
        filters.push((field.to_owned(), value.to_owned()));
    }
    Ok(filters)
}

/// Parses `alias` / `alias desc` into `(field, descending)`.
fn parse_orderby(raw: &str) -> Result<Option<(String, bool)>, DomainError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let (field, descending) = match raw.rsplit_once(' ') {
        Some((field, "desc")) => (field, true),
        Some((field, "asc")) => (field, false),
        Some(_) => {
            return Err(DomainError::Validation(format!(
                "$orderby '{raw}' must end with 'asc' or 'desc'"
            )));
        }
        None => (raw, false),
    };
    Ok(Some((field.trim().to_owned(), descending)))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn empty_params_yield_the_default_page() {
        let query = ListParams::default().to_list_query().expect("valid");
        assert!(query.filters.is_empty());
        assert_eq!(query.top, None);
        assert_eq!(query.skip, None);
    }

    #[test]
    fn filter_and_orderby_are_parsed() {
        let params: ListParams = serde_json::from_value(serde_json::json!({
            "$filter": "alias eq 'api.openai.com'",
            "$orderby": "alias desc",
            "$top": 10,
            "$skip": 5
        }))
        .expect("params");
        let query = params.to_list_query().expect("valid");
        assert_eq!(query.filter_value("alias"), Some("api.openai.com"));
        assert_eq!(query.order_by, Some(("alias".to_owned(), true)));
        assert_eq!(query.top, Some(10));
        assert_eq!(query.skip, Some(5));
    }

    #[test]
    fn unsupported_filter_operators_are_rejected() {
        let params: ListParams =
            serde_json::from_value(serde_json::json!({ "$filter": "alias ne 'x'" }))
                .expect("params");
        let err = params.to_list_query().unwrap_err();
        assert!(matches!(err, crate::domain::error::DomainError::Validation(_)));
    }

    #[test]
    fn select_splits_field_names() {
        let params: ListParams =
            serde_json::from_value(serde_json::json!({ "$select": "id,alias, server" }))
                .expect("params");
        assert_eq!(params.selected_fields(), ["id", "alias", "server"]);
    }
}
