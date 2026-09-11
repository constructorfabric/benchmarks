//! Wire representations of the configuration rows — `cpt-cf-oagw-dod-management-routes`.
//!
//! The representation of one row is the resource kind's own schema shape: the
//! domain type serialized as it is declared, with `id` carried as the resource's
//! anonymous GTS instance. A list page carries the platform page envelope plus
//! the projection the caller asked for, and the projection is applied to every
//! item on the wire.

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::control_plane::odata::Page;
use crate::domain::plugin_contract::PluginFamily;
use crate::gts;
use crate::store::{PluginRow, RouteRow, UpstreamRow};

/// The platform page envelope's row set.
const ITEMS: &str = "items";
/// The platform page envelope's paging metadata.
const PAGE_INFO: &str = "page_info";
/// The projection the page was built with, present only when one was asked for.
const PROJECTION: &str = "projection";

/// The anonymous GTS instance identifier of one upstream row.
#[must_use]
pub fn upstream_id(id: Uuid) -> String {
    gts::gts_instance(gts::UPSTREAM_TYPE, id)
}

/// The anonymous GTS instance identifier of one route row.
#[must_use]
pub fn route_id(id: Uuid) -> String {
    gts::gts_instance(gts::ROUTE_TYPE, id)
}

/// The representation of one upstream row.
#[must_use]
pub fn upstream(row: &UpstreamRow) -> Value {
    let representation = serde_json::to_value(&row.upstream).unwrap_or_default();
    identified(representation, upstream_id(row.upstream.id))
}

/// The representation of one route row.
#[must_use]
pub fn route(row: &RouteRow) -> Value {
    let representation = serde_json::to_value(&row.route).unwrap_or_default();
    identified(representation, route_id(row.route.id))
}

/// The wire page of an upstream list.
#[must_use]
pub fn upstream_page(page: &Page<UpstreamRow>) -> Value {
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| projected(&upstream(row), &page.projection))
        .collect();
    envelope(items, &page.projection, page.top)
}

/// The wire page of a route list.
#[must_use]
pub fn route_page(page: &Page<RouteRow>) -> Value {
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| projected(&route(row), &page.projection))
        .collect();
    envelope(items, &page.projection, page.top)
}

/// The anonymous GTS instance identifier of one plugin row, derived from the
/// family its `plugin_type` names.
#[must_use]
pub fn plugin_id(family: PluginFamily, id: Uuid) -> String {
    gts::gts_instance(family.base_type(), id)
}

/// The representation of one plugin row.
///
/// The configuration schema and the source are carried exactly as stored: no
/// member is re-rendered, defaulted, or dropped.
#[must_use]
pub fn plugin(row: &PluginRow) -> Value {
    let representation = serde_json::to_value(&row.plugin).unwrap_or_default();
    identified(representation, plugin_row_id(row))
}

/// The stored Starlark source of one plugin, as the source path answers it.
#[must_use]
pub fn plugin_source(source: &str) -> Value {
    Value::from(source)
}

/// The wire page of a plugin list.
#[must_use]
pub fn plugin_page(page: &Page<PluginRow>) -> Value {
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| projected(&plugin(row), &page.projection))
        .collect();
    envelope(items, &page.projection, page.top)
}

/// The anonymous GTS instance identifier one stored plugin row answers to,
/// derived from the family literal its `plugin_type` carries.
fn plugin_row_id(row: &PluginRow) -> String {
    match PluginFamily::from_type_literal(&row.plugin.plugin_type) {
        Some(family) => plugin_id(family, row.plugin.id),
        // The store's invariant check holds every stored literal to one of the
        // three families, so a row that named no family is never stored; it
        // would answer to its bare identifier.
        None => row.plugin.id.to_string(),
    }
}

/// Serializes one domain row and states its identifier as the anonymous GTS
/// instance the resource kind is addressed by.
fn identified(mut representation: Value, id: String) -> Value {
    if let Some(object) = representation.as_object_mut() {
        object.insert(String::from("id"), Value::from(id));
    }
    representation
}

/// Narrows one representation to the properties the caller projected.
///
/// An empty projection leaves the representation whole.
fn projected(representation: &Value, projection: &[String]) -> Value {
    if projection.is_empty() {
        return representation.clone();
    }
    let Some(fields) = representation.as_object() else {
        return representation.clone();
    };
    let mut narrowed = Map::new();
    for name in projection {
        if let Some(value) = fields.get(name.as_str()) {
            narrowed.insert(name.clone(), value.clone());
        }
    }
    Value::Object(narrowed)
}

/// The platform page envelope: the row set, the paging metadata, and — only
/// when the caller projected — the projection the items were narrowed to.
fn envelope(items: Vec<Value>, projection: &[String], top: u64) -> Value {
    let mut page_info = Map::new();
    page_info.insert(String::from("limit"), Value::from(top));
    page_info.insert(String::from("next_cursor"), Value::Null);
    page_info.insert(String::from("prev_cursor"), Value::Null);

    let mut body = Map::new();
    body.insert(String::from(ITEMS), Value::from(items));
    body.insert(String::from(PAGE_INFO), Value::Object(page_info));
    if !projection.is_empty() {
        body.insert(String::from(PROJECTION), Value::from(projection.to_vec()));
    }
    Value::Object(body)
}
