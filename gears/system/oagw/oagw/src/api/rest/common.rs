//! Shared REST plumbing: resource-id handling, projection and paging.

use std::collections::HashSet;

use axum::Json;
use toolkit_odata::Page;
use uuid::Uuid;

use crate::api::rest::query::ListQuery;
use crate::domain::error::DomainError;

/// Builds the anonymous GTS id of a resource instance.
#[must_use]
pub fn resource_id(type_id: &str, id: Uuid) -> String {
    crate::domain::models::gts_instance_id(type_id, id)
}

/// Parses a path identifier, accepting both the full GTS id and the bare UUID.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] when the value is neither a
/// UUID nor the expected GTS id.
pub fn parse_resource_id(type_id: &str, raw: &str) -> Result<Uuid, DomainError> {
    let raw = raw.trim();
    if let Ok(id) = raw.parse::<Uuid>() {
        return Ok(id);
    }
    if let Some(suffix) = crate::domain::models::strip_gts_prefix(type_id, raw)
        && let Ok(id) = suffix.parse::<Uuid>()
    {
        return Ok(id);
    }
    Err(DomainError::validation_with_value(
        format!("identifier must be a UUID or a '{type_id}' GTS id"),
        raw.to_owned(),
    ))
}

/// Applies `$select` to an already-serialized item.
#[must_use]
pub fn project_value(item: &serde_json::Value, query: &ListQuery) -> serde_json::Value {
    let Some(fields) = query.selected_fields() else {
        return item.clone();
    };
    if fields.is_empty() {
        return item.clone();
    }
    let selected: HashSet<String> = fields.iter().map(|field| field.to_lowercase()).collect();
    toolkit::api::select::project_json(item, &selected)
}

/// Builds the paged list response.
///
/// The list endpoints return [`toolkit::Page`] (assumption A3). Ordering of
/// the stages is filter → orderby → `$top`/`$skip` → `$select`, so a
/// `$filter` on a field that `$select` drops still works.
pub fn paged<T, F>(
    items: &[T],
    query: &ListQuery,
    serialize: F,
) -> Result<Json<Page<serde_json::Value>>, DomainError>
where
    F: Fn(&T) -> serde_json::Value,
{
    let serialized: Vec<serde_json::Value> = items
        .iter()
        .map(serialize)
        .collect();
    let kept = query.apply(serialized)?;
    let total = kept.len();
    let projected: Vec<serde_json::Value> = kept
        .iter()
        .map(|item| project_value(item, query))
        .collect();
    Ok(Json(query.page(projected, total)))
}
