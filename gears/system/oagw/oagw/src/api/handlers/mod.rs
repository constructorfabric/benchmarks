// Created: 2026-08-31 by Constructor Tech
//! REST handlers, grouped per resource.

use toolkit::Page;
use toolkit::api::response::no_content;

use crate::api::query::{ListQuery, apply_list_query, to_page};
use crate::error::{OagwError, OagwErrorKind, OagwResult};

mod plugins;
mod proxy;
mod routes;
mod upstreams;

pub(crate) use plugins::{
    create_plugin, delete_plugin, get_plugin, get_plugin_source, list_plugins,
};
pub(crate) use proxy::{proxy_alias, proxy_unregistered_method};
pub(crate) use routes::{create_route, delete_route, get_route, list_routes, update_route};
pub(crate) use upstreams::{
    create_upstream, delete_upstream, get_upstream, list_upstreams, update_upstream,
};

/// Serialize a batch of records and run the list pipeline over them.
///
/// # Errors
/// 400 for invalid query options, 500 when a DTO cannot be encoded.
pub(crate) fn serialize_all<T, D, F>(
    records: Vec<T>,
    project: F,
) -> OagwResult<Vec<serde_json::Value>>
where
    D: serde::Serialize,
    F: FnMut(T) -> D,
{
    records.into_iter().map(project).map(serialize).collect()
}

/// Apply the list query and wrap the result in the platform page envelope.
pub(crate) fn to_page_json(
    items: Vec<serde_json::Value>,
    query: &ListQuery,
) -> axum::Json<Page<serde_json::Value>> {
    let page = apply_list_query(items, query);
    axum::Json(to_page(page, query.top))
}

/// JSON-encode a DTO; an encoding failure is a control-plane bug, so it
/// surfaces as a 500 instead of being silently dropped.
fn serialize<T: serde::Serialize>(value: T) -> OagwResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| {
        OagwError::new(
            OagwErrorKind::Internal,
            format!("response encoding failed: {error}"),
        )
    })
}

/// 204 response for successful deletions.
#[must_use]
pub(crate) fn deleted() -> impl axum::response::IntoResponse {
    no_content()
}
