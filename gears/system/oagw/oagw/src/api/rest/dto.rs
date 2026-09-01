//! REST wire DTOs for the OAGW management API.
//!
//! Responses reuse the domain entities (which already serialize in the
//! documented camelCase wire shape).  List endpoints return a small
//! envelope with the items and their count.

use serde::Serialize;

use crate::domain::dto::{CustomPlugin, Route, Upstream};

/// `GET /api/oagw/v1/upstreams` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamsResponse {
    pub items: Vec<Upstream>,
    pub count: usize,
}

/// `GET /api/oagw/v1/routes` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutesResponse {
    pub items: Vec<Route>,
    pub count: usize,
}

/// `GET /api/oagw/v1/plugins` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsResponse {
    pub items: Vec<CustomPlugin>,
    pub count: usize,
}

/// Optional list pagination (`$top` / `$skip` are honored; full `OData`
/// filtering/select is future work — see report).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ListQuery {
    #[serde(default, rename = "$top")]
    pub top: Option<usize>,
    #[serde(default, rename = "$skip")]
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Apply `$skip`/`$top` to an already-ordered item list.
    #[must_use]
    pub fn paginate<T: Clone>(&self, items: Vec<T>) -> Vec<T> {
        let skip = self.skip.unwrap_or(0);
        let items = items.into_iter().skip(skip).collect::<Vec<_>>();
        match self.top {
            Some(top) => items.into_iter().take(top).collect(),
            None => items,
        }
    }
}
