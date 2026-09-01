//! REST DTOs for the OAGW management API.
//!
//! Create/update request bodies and single-resource responses reuse the
//! domain models directly (`Upstream`, `Route`, `PluginRecord`); this module
//! only defines the wrapper shapes that differ (list pages, plugin source).

use serde::Serialize;
use uuid::Uuid;

/// List response shape: `{ "items": [...], "page_info": {...} }`.
#[derive(Debug, Clone, Serialize)]
pub struct ListResponse {
    pub items: Vec<serde_json::Value>,
    pub page_info: PageInfoDto,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageInfoDto {
    pub limit: u64,
}

impl ListResponse {
    /// Wrap a page of projected items (already OData-applied).
    #[must_use]
    pub fn of(items: Vec<serde_json::Value>) -> Self {
        let limit = items.len() as u64;
        Self {
            items,
            page_info: PageInfoDto { limit },
        }
    }
}

/// Response for `GET /plugins/{id}/source`.
#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceResponse {
    pub id: Uuid,
    pub source: String,
}
