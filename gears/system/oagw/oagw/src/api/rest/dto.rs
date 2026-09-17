//! Transport DTOs for the OAGW REST layer.
//!
//! Resource bodies are the domain wire models directly (`Upstream`, `Route`,
//! `Plugin` — they mirror the JSON schemas with `deny_unknown_fields` and
//! `serde(default)`). This module adds the transport-only shapes: list
//! response envelopes, the serialized `$select` projection, list-query
//! parameter extraction (`$filter`, `$select`, `$orderby`, `$top`, `$skip`
//! with default 50 / max 100) and the plugin-source envelope.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::dto::ListParams;
use crate::domain::error::{OagwError, OagwResult};

/// OData-style list query parameters extracted from the raw query string.
///
/// Parameters are parsed as strings so friendly `400` messages can be
/// produced for malformed `$top`/`$skip` instead of an axum rejection.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
    #[serde(rename = "$top", default)]
    pub top: Option<String>,
    #[serde(rename = "$skip", default)]
    pub skip: Option<String>,
}

impl ListQuery {
    /// Convert into the domain list params, validating numeric fields.
    ///
    /// # Errors
    ///
    /// `400` when `$top`/`$skip` are not non-negative integers.
    pub fn into_domain(self) -> OagwResult<ListParams> {
        Ok(ListParams {
            filter: self.filter,
            select: self.select,
            orderby: self.orderby,
            top: parse_count(self.top.as_deref(), "$top")?,
            skip: parse_count(self.skip.as_deref(), "$skip")?,
        })
    }
}

fn parse_count(raw: Option<&str>, param: &str) -> OagwResult<Option<u32>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: u64 = trimmed.parse().map_err(|_| OagwError::Validation {
        detail: format!("{param} must be a non-negative integer, got {raw:?}"),
    })?;
    if value > u32::MAX as u64 {
        return Err(OagwError::Validation {
            detail: format!("{param} value {value} is out of range"),
        });
    }
    Ok(Some(value as u32))
}

/// List response envelope: total count + projected (or full) item objects.
#[derive(Debug, Clone, Serialize)]
pub struct ListBody {
    /// Total number of matching records (before `$top`/`$skip`).
    pub total: usize,
    /// Paged records; each item respects `$select` when present.
    pub items: Vec<Value>,
}

impl ListBody {
    /// Build the envelope from records, applying the `$select` projection.
    #[must_use]
    pub fn from_records(records: Vec<Value>, total: usize, select: Option<&str>) -> Self {
        let items = match select {
            Some(select) if !select.trim().is_empty() => {
                let fields: Vec<&str> = select
                    .split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .collect();
                records.into_iter().map(|v| project(&v, &fields)).collect()
            }
            _ => records,
        };
        Self { total, items }
    }
}

/// Keep only the requested top-level fields of a record object.
fn project(value: &Value, fields: &[&str]) -> Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for field in fields {
        if let Some(v) = obj.get(*field) {
            out.insert((*field).to_owned(), v.clone());
        }
    }
    Value::Object(out)
}

/// Serialized Starlark source for `GET /plugins/{id}/source`.
#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceBody {
    /// The stored Starlark source code.
    pub source: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn select_projects_only_requested_fields() {
        let value = serde_json::json!({
            "id": "uuid-1",
            "alias": "api.example.com",
            "enabled": true,
            "server": { "endpoints": [] }
        });
        let body = ListBody::from_records(vec![value], 1, Some("id,alias"));
        assert_eq!(body.total, 1);
        let item = body.items[0].as_object().unwrap();
        assert_eq!(item.len(), 2);
        assert!(item.contains_key("id"));
        assert!(item.contains_key("alias"));
        assert!(!item.contains_key("enabled"));
        assert!(!item.contains_key("server"));
    }

    #[test]
    fn without_select_full_records_returned() {
        let value = serde_json::json!({ "id": "u", "alias": "a", "enabled": true });
        let body = ListBody::from_records(vec![value], 1, None);
        assert_eq!(body.items[0].as_object().unwrap().len(), 3);
    }

    #[test]
    fn top_and_skip_parsed() {
        let q: ListQuery = serde_json::from_value(serde_json::json!({
            "$top": "10",
            "$skip": "5"
        }))
        .unwrap();
        let p = q.into_domain().unwrap();
        assert_eq!(p.top, Some(10));
        assert_eq!(p.skip, Some(5));
    }

    #[test]
    fn bad_top_rejected() {
        let q: ListQuery = serde_json::from_value(serde_json::json!({ "$top": "abc" })).unwrap();
        assert!(q.into_domain().is_err());
    }
}
