//! REST DTOs.
//!
//! The wire shapes **are** the domain resource shapes (`docs/schemas/*.v1.json`),
//! so the handlers exchange [`Upstream`], [`Route`] and [`Plugin`] directly; the
//! only DTOs defined here are the OData query parameters and the aggregate
//! wrappers the management API returns.

use serde::Deserialize;

use crate::domain::services::ListParams;

pub use crate::domain::model::{Plugin, Route, Upstream};

/// The OData parameters the list endpoints honour.
#[derive(Debug, Clone, Default, Deserialize, toolkit::QueryParams)]
pub struct ListQuery {
    /// `$filter` — reserved; accepted and ignored.
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    /// `$select` — reserved; accepted and ignored.
    #[serde(rename = "$select")]
    pub select: Option<String>,
    /// `$orderby` — `created_at` or `updated_at`, optionally ` asc` / ` desc`.
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    /// `$top` — page size, default 50, max 100.
    #[serde(rename = "$top")]
    pub top: Option<usize>,
    /// `$skip` — entries to skip.
    #[serde(rename = "$skip")]
    pub skip: Option<usize>,
}

impl ListQuery {
    /// The domain pagination parameters.
    #[must_use]
    pub fn params(&self) -> ListParams {
        ListParams {
            skip: self.skip.unwrap_or(0),
            top: self.top,
            orderby: self.orderby.clone(),
        }
    }
}

impl toolkit::api::api_dto::RequestApiDto for Upstream {}
impl toolkit::api::api_dto::ResponseApiDto for Upstream {}
impl toolkit::api::api_dto::RequestApiDto for Route {}
impl toolkit::api::api_dto::ResponseApiDto for Route {}
impl toolkit::api::api_dto::RequestApiDto for Plugin {}
impl toolkit::api::api_dto::ResponseApiDto for Plugin {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_parameters_map_to_the_domain_list_params() {
        let query: ListQuery = serde_json::from_value(
            serde_json::json!({ "$top": 10, "$skip": 5, "$orderby": "created_at desc" }),
        )
        .expect("query");
        let params = query.params();
        assert_eq!(params.skip, 5);
        assert_eq!(params.top, Some(10));
        assert_eq!(params.orderby.as_deref(), Some("created_at desc"));
    }

    #[test]
    fn the_odata_dollar_prefixes_are_accepted_verbatim() {
        let query: ListQuery =
            serde_json::from_value(serde_json::json!({ "$filter": "enabled eq true" }))
                .expect("query");
        assert_eq!(query.filter.as_deref(), Some("enabled eq true"));
    }
}
