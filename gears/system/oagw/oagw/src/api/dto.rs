//! Management API response envelopes.

use serde::Serialize;

use super::query::ListQuery;

/// A page of a list response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ListResponse {
    /// The resources on this page.
    pub items: Vec<serde_json::Value>,
    /// Number of resources returned.
    pub count: usize,
    /// Number of resources matching the query before paging.
    pub total: usize,
}

/// Applies a list query to a set of serialized resources.
#[must_use]
pub fn list(items: Vec<serde_json::Value>, query: &ListQuery) -> ListResponse {
    let total = items.len();
    let page = query.apply(items);
    ListResponse {
        count: page.len(),
        total,
        items: page,
    }
}

impl toolkit::api::api_dto::ResponseApiDto for ListResponse {}
