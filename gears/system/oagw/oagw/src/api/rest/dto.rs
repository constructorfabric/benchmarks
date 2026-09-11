//! Wire DTOs of the OAGW REST surface.
//!
//! The resource shapes (`UpstreamSpec`, and later routes and plugins) are the
//! contract of `docs/schemas/upstream.v1.schema.json` and live in
//! [`crate::domain::model`] so the control plane and the transport share one
//! definition. This module adds what only the transport layer needs: the
//! `OData` list-query parameters.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::OagwError;
/// Default page size of a list operation (DESIGN §3.3: `$top` defaults to 50).
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size of a list operation (DESIGN §3.3: `$top` is capped at 100).
pub const MAX_TOP: usize = 100;

/// `OData` list query parameters (`$top` / `$skip`).
///
/// Values that are not non-negative integers are ignored, so a list call never
/// fails because of a paging option; `$filter` is evaluated (see
/// [`ListFilter`]) and `$select` / `$orderby` are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListParams {
    /// Maximum number of returned items.
    pub top: usize,
    /// Number of items to skip before the first returned item.
    pub skip: usize,
}

impl ListParams {
    /// The default parameters: the first 50 items.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            top: DEFAULT_TOP,
            skip: 0,
        }
    }

    /// Extract `$top` / `$skip` from the parsed query string.
    #[must_use]
    pub fn from_query(query: &HashMap<String, String>) -> Self {
        let top = query
            .get("$top")
            .and_then(|value| value.parse::<usize>().ok())
            .map_or(DEFAULT_TOP, |value| value.min(MAX_TOP));
        let skip = query
            .get("$skip")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        Self { top, skip }
    }
}

impl Default for ListParams {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `OData` `$filter` expression of the form `field eq 'value'`.
///
/// DESIGN "List Query Parameters" names `$filter=upstream_id eq '{uuid}'` for
/// the route list; this is the one form the list endpoints evaluate, anded
/// across several comma- or space-separated comparisons. `$select` and
/// `$orderby` are **not** implemented: they are dropped, so a list call that
/// carries them returns the full representation in the documented order rather
/// than failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFilter {
    /// `(field, value)` comparisons, all of which must hold.
    pub clauses: Vec<(String, String)>,
}

impl ListFilter {
    /// Parse a raw `$filter` value.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the expression is not a non-empty list
    /// of `field eq 'value'` comparisons: an operator this module does not
    /// evaluate is reported rather than silently returning the unfiltered
    /// collection, which would read as a wrong answer instead of an
    /// unsupported one.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        let unsupported = || {
            OagwError::validation(format!(
                "`$filter` supports `field eq 'value'` comparisons only; got `{raw}`"
            ))
            .with_invalid_value(raw.to_owned())
        };
        let text = raw.trim();
        if text.is_empty() {
            return Err(unsupported());
        }
        let mut clauses = Vec::new();
        for comparison in text.split(" and ") {
            let comparison = comparison.trim();
            let Some((field, value)) = comparison.split_once(" eq ") else {
                return Err(unsupported());
            };
            let field = field.trim();
            let value = value.trim().trim_matches('\'');
            if field.is_empty() || value.is_empty() {
                return Err(unsupported());
            }
            clauses.push((field.to_owned(), value.to_owned()));
        }
        Ok(Self { clauses })
    }

    /// Whether a document satisfies every clause.
    #[must_use]
    pub fn matches(&self, document: &Value) -> bool {
        self.clauses.iter().all(|(field, value)| {
            document
                .get(field.as_str())
                .and_then(Value::as_str)
                .is_some_and(|member| member == value)
        })
    }
}

/// Extract `$top`, `$skip` and `$filter` from the parsed query string.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when `$filter` is present but not a supported
/// expression.
pub fn list_params(
    query: &HashMap<String, String>,
) -> Result<(ListParams, Option<ListFilter>), OagwError> {
    let filter = match query.get("$filter") {
        Some(raw) => Some(ListFilter::parse(raw)?),
        None => None,
    };
    Ok((ListParams::from_query(query), filter))
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
