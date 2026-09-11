// Updated: 2026-09-01 by Constructor Tech
//! OData-style list options: `$filter`, `$select`, `$orderby`, `$top`, `$skip`.
//!
//! Hand-rolled rather than taken from the platform's `OData` extractor because
//! that one rejects `$skip`, which the API's paging contract requires.

use serde::Deserialize;

/// Parsed list options.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListOptions {
    pub filter: Option<String>,
    pub select: Option<String>,
    pub orderby: Option<String>,
    pub top: Option<usize>,
    pub skip: Option<usize>,
}

impl ListOptions {
    /// Effective page size, clamped to the API's ceiling.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.top.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Effective offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.skip.unwrap_or(0)
    }

    /// Whether a `$filter` names a tag, in the form `tags eq 'value'`.
    #[must_use]
    pub fn tag_filter(&self) -> Option<String> {
        let raw = self.filter.as_deref()?;
        let rest = raw
            .strip_prefix("tags eq ")
            .or_else(|| raw.strip_prefix("tags eq'"))?;
        let value = rest.trim().trim_matches('\'').trim_matches('"');
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// Whether `$orderby` asks for a descending sort on the given field.
    #[must_use]
    pub fn descending(&self) -> bool {
        self.orderby
            .as_deref()
            .map(str::trim)
            .is_some_and(|o| o.ends_with(" desc"))
    }

    /// A `$filter` naming a plugin kind, in the form `type eq 'guard'`.
    #[must_use]
    pub fn kind_filter(&self) -> Option<crate::domain::dto::PluginKind> {
        let raw = self.filter.as_deref()?;
        for prefix in ["type eq ", "kind eq "] {
            let Some(rest) = raw.strip_prefix(prefix) else {
                continue;
            };
            let value = rest.trim().trim_matches('\'').trim_matches('"');
            if let Some(kind) = crate::domain::dto::PluginKind::parse(value) {
                return Some(kind);
            }
        }
        None
    }
}

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 100;

/// Raw query string form, as it arrives on the wire.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawListOptions {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<usize>,
    #[serde(rename = "$skip")]
    pub skip: Option<usize>,
}

impl From<RawListOptions> for ListOptions {
    fn from(raw: RawListOptions) -> Self {
        Self {
            filter: raw.filter,
            select: raw.select,
            orderby: raw.orderby,
            top: raw.top,
            skip: raw.skip,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_the_query_is_empty() {
        let o = ListOptions::default();
        assert_eq!(o.limit(), 50);
        assert_eq!(o.offset(), 0);
    }

    #[test]
    fn top_is_clamped_to_the_ceiling() {
        let raw: RawListOptions =
            serde_json::from_value(serde_json::json!({ "$top": 1_000 })).unwrap();
        assert_eq!(ListOptions::from(raw).limit(), 100);
    }

    #[test]
    fn skip_is_honoured() {
        let raw: RawListOptions =
            serde_json::from_value(serde_json::json!({ "$skip": 25 })).unwrap();
        assert_eq!(ListOptions::from(raw).offset(), 25);
    }

    #[test]
    fn a_tag_filter_is_parsed_out_of_the_filter_clause() {
        let raw: RawListOptions =
            serde_json::from_value(serde_json::json!({ "$filter": "tags eq 'ai'" })).unwrap();
        assert_eq!(ListOptions::from(raw).tag_filter().as_deref(), Some("ai"));
    }

    #[test]
    fn orderby_direction_is_recognised() {
        let raw: RawListOptions =
            serde_json::from_value(serde_json::json!({ "$orderby": "alias desc" })).unwrap();
        assert!(ListOptions::from(raw).descending());
    }
}
