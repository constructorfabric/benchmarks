// Created: 2026-09-01 by Constructor Tech
//! Management API handlers.
//!
//! `docs/DESIGN.md` §3.3. Strictly tenant-scoped: an ancestor's resource is
//! indistinguishable from a missing one, per the visibility table there.

use std::collections::BTreeMap;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use crate::api::dto::{PluginDto, PluginSource, RouteDto, UpstreamDto};
use crate::api::state::OagwState;
use crate::domain::errors::{OagwError, Result};
use crate::domain::model;
use crate::domain::store::{plugin_bound_to_route, plugin_bound_to_upstream};
use crate::domain::{alias, validate_route, validate_upstream};

/// The caller's tenant.
fn tenant(ctx: &SecurityContext) -> String {
    ctx.subject_tenant_id().to_string()
}

/// The bare key of a resource reference, accepting either a bare UUID or the
/// full GTS identifier.
fn key_of(raw: &str) -> String {
    model::uuid_from_resource_id(raw).unwrap_or_else(|| raw.to_owned())
}

/// The upstream a reference names, accepting its id, its bare UUID or its
/// alias. Only this tenant's resources resolve.
fn resolve_upstream(state: &OagwState, tenant_id: &str, raw: &str) -> Option<model::Upstream> {
    state
        .store
        .get_upstream(tenant_id, &key_of(raw))
        .or_else(|| state.store.get_upstream_by_alias(tenant_id, raw))
}

// ---- upstreams ---------------------------------------------------------

/// List this tenant's upstreams.
pub async fn list_upstreams(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let mut items: Vec<UpstreamDto> = state
        .store
        .list_upstreams(&tenant_id)
        .iter()
        .map(UpstreamDto::from_domain)
        .collect();
    if let Some(filter) = query.get("$filter").or_else(|| query.get("filter")) {
        retain_alias_filter(&mut items, filter);
    }
    Ok(axum::Json(items))
}

fn retain_alias_filter(items: &mut Vec<UpstreamDto>, filter: &str) {
    let Some((field, value)) = filter.trim().split_once(" eq ") else {
        return;
    };
    let value = value.trim().trim_matches('\'').to_ascii_lowercase();
    if field.trim().eq_ignore_ascii_case("alias") {
        items.retain(|u| {
            u.alias
                .as_deref()
                .unwrap_or_default()
                .eq_ignore_ascii_case(&value)
        });
    }
}

/// Create an upstream.
pub async fn create_upstream(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(dto): axum::Json<UpstreamDto>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let id = model::gts_resource_id("upstream", &crate::domain::new_uuid());
    let mut candidate = dto.into_domain(&id, &tenant_id);
    let provided = (!candidate.alias.is_empty()).then_some(candidate.alias.as_str());
    candidate.alias = alias::enforce_alias(provided, &candidate.server.endpoints)
        .map_err(OagwError::validation_error)?;
    validate_upstream(&candidate)?;
    let alias = candidate.alias.clone();
    let stored = state
        .store
        .insert_upstream(candidate)
        .map_err(|_| OagwError::alias_conflict(&alias))?;
    Ok((
        StatusCode::CREATED,
        axum::Json(UpstreamDto::from_domain(&stored)),
    ))
}

/// Read one upstream.
pub async fn get_upstream(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let found = find_upstream(&state, &tenant_id, &id)
        .ok_or_else(|| OagwError::route_not_found(format!("upstream '{id}' not found")))?;
    Ok(axum::Json(UpstreamDto::from_domain(&found)))
}

/// The upstream `raw` names — by id, bare UUID or alias.
fn find_upstream(state: &OagwState, tenant_id: &str, raw: &str) -> Option<model::Upstream> {
    state
        .store
        .get_upstream(tenant_id, &key_of(raw))
        .or_else(|| state.store.get_upstream_by_alias(tenant_id, raw))
}

/// Replace an upstream.
pub async fn update_upstream(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    axum::Json(dto): axum::Json<UpstreamDto>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let existing = find_upstream(&state, &tenant_id, &id)
        .ok_or_else(|| OagwError::route_not_found(format!("upstream '{id}' not found")))?;
    let mut candidate = dto.into_domain(&existing.id, &tenant_id);
    let provided = (!candidate.alias.is_empty()).then_some(candidate.alias.as_str());
    candidate.alias = alias::enforce_alias_update(
        &existing.alias,
        provided,
        &existing.server.endpoints,
        &candidate.server.endpoints,
    )
    .map_err(OagwError::validation_error)?;
    validate_upstream(&candidate)?;
    state
        .store
        .put_upstream(candidate.clone())
        .map_err(|_| OagwError::alias_conflict(&candidate.alias))?;
    Ok(axum::Json(UpstreamDto::from_domain(&candidate)))
}

/// Delete an upstream, and its routes with it.
pub async fn delete_upstream(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = find_upstream(&state, &tenant_id, &id)
        .ok_or_else(|| OagwError::route_not_found(format!("upstream '{id}' not found")))?
        .id;
    state
        .store
        .delete_upstream(&tenant_id, &key)
        .ok_or_else(|| OagwError::route_not_found(format!("upstream '{id}' not found")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- routes ------------------------------------------------------------

/// List this tenant's routes.
pub async fn list_routes(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let items: Vec<RouteDto> = state
        .store
        .list_routes(&tenant_id)
        .iter()
        .map(RouteDto::from_domain)
        .collect();
    let _ = query;
    Ok(axum::Json(items))
}

/// Create a route.
pub async fn create_route(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(dto): axum::Json<RouteDto>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let id = model::gts_resource_id("route", &crate::domain::new_uuid());
    let mut candidate = dto.into_domain(&id, &tenant_id);
    validate_route(&candidate)?;
    let upstream =
        resolve_upstream(&state, &tenant_id, &candidate.upstream_id).ok_or_else(|| {
            OagwError::validation_error(format!(
                "upstream '{}' does not belong to this tenant",
                candidate.upstream_id
            ))
        })?;
    candidate.upstream_id = upstream.id;
    let stored = state.store.insert_route(candidate).map_err(|detail| {
        OagwError::match_conflict(
            "a route with the same path, priority and methods already exists for this upstream",
        )
        .with_detail(detail)
    })?;
    Ok((
        StatusCode::CREATED,
        axum::Json(RouteDto::from_domain(&stored)),
    ))
}

/// Read one route.
pub async fn get_route(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = key_of(&id);
    let found = state
        .store
        .get_route(&tenant_id, &key)
        .ok_or_else(|| OagwError::route_not_found(format!("route '{id}' not found")))?;
    Ok(axum::Json(RouteDto::from_domain(&found)))
}

/// Replace a route. `upstream_id` is immutable and is taken from the stored one.
pub async fn update_route(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    axum::Json(dto): axum::Json<RouteDto>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = key_of(&id);
    let existing = state
        .store
        .get_route(&tenant_id, &key)
        .ok_or_else(|| OagwError::route_not_found(format!("route '{id}' not found")))?;
    let mut candidate = dto.into_domain(&existing.id, &tenant_id);
    candidate.upstream_id = existing.upstream_id.clone();
    validate_route(&candidate)?;
    state.store.put_route(candidate.clone());
    Ok(axum::Json(RouteDto::from_domain(&candidate)))
}

/// Delete a route.
pub async fn delete_route(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = key_of(&id);
    state
        .store
        .delete_route(&tenant_id, &key)
        .ok_or_else(|| OagwError::route_not_found(format!("route '{id}' not found")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- plugins -----------------------------------------------------------

/// List this tenant's custom plugins.
pub async fn list_plugins(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    Ok(axum::Json(
        state
            .store
            .list_plugins(&tenant_id)
            .iter()
            .map(PluginDto::from_domain)
            .collect::<Vec<_>>(),
    ))
}

/// Create a custom plugin.
pub async fn create_plugin(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(dto): axum::Json<PluginDto>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let kind = plugin_kind(&dto.plugin_type);
    let id = model::gts_resource_id(&format!("{kind}_plugin"), &crate::domain::new_uuid());
    let mut candidate = dto.into_domain(&id, &tenant_id);
    candidate.plugin_type = kind;
    let stored = state.store.insert_plugin(candidate);
    Ok((
        StatusCode::CREATED,
        axum::Json(PluginDto::from_domain(&stored)),
    ))
}

/// Accept `auth`, `gts.cf.core.oagw.auth_plugin.v1` or the full identifier.
fn plugin_kind(raw: &str) -> String {
    for kind in ["auth", "guard", "transform"] {
        if raw.contains(&format!("{kind}_plugin")) || raw.eq_ignore_ascii_case(kind) {
            return kind.to_owned();
        }
    }
    "guard".to_owned()
}

/// Read one plugin.
pub async fn get_plugin(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = key_of(&id);
    let found = state
        .store
        .get_plugin(&tenant_id, &key)
        .ok_or_else(|| OagwError::route_not_found(format!("plugin '{id}' not found")))?;
    Ok(axum::Json(PluginDto::from_domain(&found)))
}

/// Delete a plugin, refusing while anything still binds it.
pub async fn delete_plugin(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let key = key_of(&id);
    let deleted = state
        .store
        .delete_plugin(&tenant_id, &key)
        .map_err(|refs| {
            OagwError::plugin_in_use(format!(
                "plugin is referenced by {} resource(s): {}",
                refs.len(),
                refs.join(", ")
            ))
        })?;
    state
        .store
        .mark_unlinked_if_orphaned(&tenant_id, &deleted.id);
    Ok(StatusCode::NO_CONTENT)
}

/// Where a plugin is referenced, and its source.
pub async fn plugin_source(
    Extension(state): Extension<OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let tenant_id = tenant(&ctx);
    let plugin = state
        .store
        .get_plugin(&tenant_id, &id)
        .ok_or_else(|| OagwError::route_not_found(format!("plugin '{id}' not found")))?;
    let key = plugin.id.clone();
    let tables = state
        .store
        .tenant_tables(&tenant_id)
        .ok_or_else(|| OagwError::route_not_found(format!("plugin '{id}' not found")))?;
    let tables = tables.read();
    let mut source = PluginSource {
        plugin_id: key.clone(),
        origin: "custom".to_owned(),
        upstreams: Vec::new(),
        routes: Vec::new(),
        source: plugin.source.clone(),
    };
    for upstream in tables.upstreams.values() {
        if plugin_bound_to_upstream(upstream, &key) {
            source.upstreams.push(upstream.id.clone());
        }
    }
    for route in tables.routes.values() {
        if plugin_bound_to_route(route, &key) {
            source.routes.push(route.id.clone());
        }
    }
    Ok(axum::Json(source))
}
