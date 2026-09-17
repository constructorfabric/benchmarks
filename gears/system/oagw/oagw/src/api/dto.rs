// Created: 2026-09-03 by Constructor Tech
//! Wire DTOs of the OAGW REST surface.
//!
//! The domain models in [`crate::model`] double as wire types: their serde
//! attributes are already tuned for the documented request/response shapes.
//! This module only adds the list envelope used by the collection endpoints
//! and binds every exchanged type into the toolkit API-DTO contract.

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// Envelope of a collection response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ListEnvelope {
    /// Page of results, projected by `$select` when requested.
    pub items: Vec<Value>,
    /// Number of items in this page.
    pub count: usize,
    /// Effective page size.
    pub top: usize,
    /// Effective skip.
    pub skip: usize,
}

impl From<crate::odata::ListEnvelope> for ListEnvelope {
    fn from(envelope: crate::odata::ListEnvelope) -> Self {
        Self {
            items: envelope.items,
            count: envelope.count,
            top: envelope.top,
            skip: envelope.skip,
        }
    }
}

impl toolkit::api::api_dto::ResponseApiDto for ListEnvelope {}

impl toolkit::api::api_dto::RequestApiDto for crate::model::UpstreamInput {}
impl toolkit::api::api_dto::ResponseApiDto for crate::model::Upstream {}

impl toolkit::api::api_dto::RequestApiDto for crate::model::RouteInput {}
impl toolkit::api::api_dto::ResponseApiDto for crate::model::Route {}

impl toolkit::api::api_dto::RequestApiDto for crate::model::PluginInput {}
impl toolkit::api::api_dto::ResponseApiDto for crate::model::PluginRecord {}

/// Renders a collection with the OData options of the request.
///
/// # Errors
/// Returns a problem document when the query options are invalid.
pub fn list_page<T: Serialize>(
    items: Vec<T>,
    query: Option<&str>,
    top_default: usize,
    top_max: usize,
) -> Result<ListEnvelope, crate::error::OagwError> {
    let params = crate::odata::ListParams::parse(query)?;
    let envelope = crate::odata::render(items, &params, top_default, top_max)?;
    Ok(envelope.into())
}
