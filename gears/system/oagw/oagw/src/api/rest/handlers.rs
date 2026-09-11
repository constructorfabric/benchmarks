//! Control-plane handlers: upstream, route and plugin management.
//!
//! Realizes the actor flows of `upstream-management.md`, `route-management.md`
//! and `plugin-management.md`.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    ListParams, PluginCreate, ReferencedBy, RouteCreate, RouteReplace, UpstreamWrite,
};
use crate::domain::error::DomainError;
use crate::domain::model::{PluginDef, PluginKind, Route, Upstream};
use crate::domain::tenant::Caller;
use crate::domain::{alias, plugins, validate};
use crate::gear::OagwState;

type St = Extension<Arc<OagwState>>;
type Sec = Option<Extension<SecurityContext>>;

fn caller(sec: &Sec) -> Caller {
    Caller::from_context(sec.as_ref().map(|e| &e.0))
}

/// Validate the shared parts of an upstream write.
fn validate_upstream_write(
    state: &OagwState,
    tenant: Uuid,
    w: &UpstreamWrite,
) -> Result<(), DomainError> {
    validate::validate_tags(&w.tags)?;
    validate::validate_server(&w.server)?;
    validate::validate_protocol(&w.protocol)?;
    if let Some(rl) = &w.rate_limit {
        validate::validate_rate_limit("rate_limit", rl)?;
    }
    if let Some(c) = &w.cors {
        validate::validate_cors("cors", c)?;
    }
    let exists = |id: Uuid| state.store.get_plugin(tenant, id).map(|p| p.plugin_type);
    for (i, item) in w.plugins.items.iter().enumerate() {
        plugins::resolve_binding("plugins.items", i, item, exists)?;
    }
    if let Some(t) = w.auth.plugin_type.as_deref() {
        plugins::resolve_auth_plugin(t, exists)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
// @cpt-begin:cpt-cf-oagw-dod-um-create:p1:inst-full
pub(crate) async fn create_upstream(
    Extension(state): St,
    sec: Sec,
    Json(w): Json<UpstreamWrite>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    validate_upstream_write(&state, c.tenant_id, &w)?;
    let resolved = alias::resolve(&w.server.endpoints, w.alias.as_deref())?;

    let up = Upstream {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        enabled: w.enabled,
        alias: resolved,
        tags: w.tags,
        server: w.server,
        protocol: w.protocol,
        auth: w.auth,
        headers: w.headers,
        plugins: w.plugins,
        rate_limit: w.rate_limit,
        cors: w.cors,
    };
    let stored = state.store.insert_upstream(up)?;
    Ok((StatusCode::CREATED, Json(stored)))
}
// @cpt-end:cpt-cf-oagw-dod-um-create:p1:inst-full

/// `GET /oagw/v1/upstreams`
pub(crate) async fn list_upstreams(
    Extension(state): St,
    sec: Sec,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let all = state.store.list_upstreams(c.tenant_id);
    Ok((StatusCode::OK, Json(params.paginate(&all))))
}

/// `GET /oagw/v1/upstreams/{id}`
pub(crate) async fn get_upstream(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let up = state
        .store
        .get_upstream(c.tenant_id, id)
        .ok_or_else(|| DomainError::not_found("upstream", id.to_string()))?;
    Ok((StatusCode::OK, Json(up)))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// A full replacement. The alias is immutable: an endpoint change that would
/// alter the derived alias is rejected, independent of what the body's `alias`
/// field literally says.
// @cpt-begin:cpt-cf-oagw-dod-um-replace:p1:inst-full
pub(crate) async fn replace_upstream(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
    Json(w): Json<UpstreamWrite>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let existing = state
        .store
        .get_upstream(c.tenant_id, id)
        .ok_or_else(|| DomainError::not_found("upstream", id.to_string()))?;
    validate_upstream_write(&state, c.tenant_id, &w)?;

    // Recompute the alias from the replacement's endpoints rather than
    // trusting the body's `alias` field.
    let recomputed = alias::resolve(&w.server.endpoints, w.alias.as_deref())?;
    if recomputed != existing.alias {
        return Err(DomainError::validation(
            "alias",
            format!(
                "alias is immutable; this replacement would change it from `{}` to `{recomputed}` \
                 — delete and re-create instead",
                existing.alias
            ),
        )
        .into());
    }

    let up = Upstream {
        id: existing.id,
        tenant_id: c.tenant_id,
        enabled: w.enabled,
        alias: existing.alias,
        tags: w.tags,
        server: w.server,
        protocol: w.protocol,
        auth: w.auth,
        headers: w.headers,
        plugins: w.plugins,
        rate_limit: w.rate_limit,
        cors: w.cors,
    };
    Ok((StatusCode::OK, Json(state.store.replace_upstream(up)?)))
}
// @cpt-end:cpt-cf-oagw-dod-um-replace:p1:inst-full

/// `DELETE /oagw/v1/upstreams/{id}`
pub(crate) async fn delete_upstream(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    state.store.delete_upstream(c.tenant_id, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Reject a route whose match collides with an existing enabled sibling.
fn check_match_determinism(
    state: &OagwState,
    upstream_id: Uuid,
    candidate: &Route,
) -> Result<(), DomainError> {
    let Some(ch) = candidate.match_.http.as_ref() else {
        return Ok(());
    };
    if !candidate.enabled {
        return Ok(());
    }
    for existing in state.store.routes_for_upstream(upstream_id) {
        if existing.id == candidate.id || !existing.enabled {
            continue;
        }
        let Some(eh) = existing.match_.http.as_ref() else {
            continue;
        };
        if eh.path != ch.path {
            continue;
        }
        let overlap = ch
            .methods
            .iter()
            .any(|m| eh.methods.iter().any(|e| e.eq_ignore_ascii_case(m)));
        if overlap {
            return Err(DomainError::Conflict {
                message: format!(
                    "another enabled route under this upstream already matches `{}` for one of {:?}",
                    ch.path, ch.methods
                ),
            });
        }
    }
    Ok(())
}

fn validate_route_policy(
    state: &OagwState,
    tenant: Uuid,
    tags: &[String],
    bindings: &crate::domain::model::PluginBindings,
    rate_limit: Option<&crate::domain::model::RateLimit>,
) -> Result<(), DomainError> {
    validate::validate_tags(tags)?;
    if let Some(rl) = rate_limit {
        validate::validate_rate_limit("rate_limit", rl)?;
    }
    let exists = |id: Uuid| state.store.get_plugin(tenant, id).map(|p| p.plugin_type);
    for (i, item) in bindings.items.iter().enumerate() {
        plugins::resolve_binding("plugins.items", i, item, exists)?;
    }
    Ok(())
}

/// `POST /oagw/v1/routes`
// @cpt-begin:cpt-cf-oagw-dod-rm-match-determinism:p1:inst-full
pub(crate) async fn create_route(
    Extension(state): St,
    sec: Sec,
    Json(w): Json<RouteCreate>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    // An unresolvable parent is a validation error, not a 404.
    if state.store.get_upstream(c.tenant_id, w.upstream_id).is_none() {
        return Err(DomainError::validation(
            "upstream_id",
            format!("`{}` does not resolve to a visible upstream", w.upstream_id),
        )
        .into());
    }
    validate::validate_match(&w.match_)?;
    validate_route_policy(&state, c.tenant_id, &w.tags, &w.plugins, w.rate_limit.as_ref())?;

    let route = Route {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        enabled: w.enabled,
        tags: w.tags,
        upstream_id: w.upstream_id,
        match_: w.match_,
        plugins: w.plugins,
        rate_limit: w.rate_limit,
    };
    check_match_determinism(&state, w.upstream_id, &route)?;
    Ok((StatusCode::CREATED, Json(state.store.insert_route(route))))
}
// @cpt-end:cpt-cf-oagw-dod-rm-match-determinism:p1:inst-full

/// `GET /oagw/v1/routes`
pub(crate) async fn list_routes(
    Extension(state): St,
    sec: Sec,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let all = state.store.list_routes(c.tenant_id);
    Ok((StatusCode::OK, Json(params.paginate(&all))))
}

/// `GET /oagw/v1/routes/{id}`
pub(crate) async fn get_route(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let r = state
        .store
        .get_route(c.tenant_id, id)
        .ok_or_else(|| DomainError::not_found("route", id.to_string()))?;
    Ok((StatusCode::OK, Json(r)))
}

/// `PUT /oagw/v1/routes/{id}`
pub(crate) async fn replace_route(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
    Json(w): Json<RouteReplace>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let existing = state
        .store
        .get_route(c.tenant_id, id)
        .ok_or_else(|| DomainError::not_found("route", id.to_string()))?;
    validate::validate_match(&w.match_)?;
    validate_route_policy(&state, c.tenant_id, &w.tags, &w.plugins, w.rate_limit.as_ref())?;

    let route = Route {
        id: existing.id,
        tenant_id: c.tenant_id,
        enabled: w.enabled,
        tags: w.tags,
        upstream_id: existing.upstream_id,
        match_: w.match_,
        plugins: w.plugins,
        rate_limit: w.rate_limit,
    };
    check_match_determinism(&state, existing.upstream_id, &route)?;
    Ok((StatusCode::OK, Json(state.store.replace_route(route)?)))
}

/// `DELETE /oagw/v1/routes/{id}`
pub(crate) async fn delete_route(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    state.store.delete_route(c.tenant_id, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugin definitions
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
// @cpt-begin:cpt-cf-oagw-dod-pm-custom-crud:p1:inst-full
pub(crate) async fn create_plugin(
    Extension(state): St,
    sec: Sec,
    Json(w): Json<PluginCreate>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    if w.name.trim().is_empty() {
        return Err(DomainError::validation("name", "name must not be empty").into());
    }
    if w.source_code.trim().is_empty() {
        return Err(
            DomainError::validation("source_code", "source text must not be empty").into(),
        );
    }
    if !(w.config_schema.is_null() || w.config_schema.is_object()) {
        return Err(DomainError::validation(
            "config_schema",
            "config_schema must be an object when present",
        )
        .into());
    }
    let p = PluginDef {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        name: w.name,
        description: w.description,
        plugin_type: w.plugin_type,
        config_schema: w.config_schema,
        source_code: w.source_code,
    };
    Ok((StatusCode::CREATED, Json(state.store.insert_plugin(p)?)))
}
// @cpt-end:cpt-cf-oagw-dod-pm-custom-crud:p1:inst-full

/// `GET /oagw/v1/plugins`
///
/// One merged listing: the built-in catalog (marking which entries are served)
/// followed by the calling tenant's own custom definitions.
pub(crate) async fn list_plugins(
    Extension(state): St,
    sec: Sec,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    let mut all: Vec<serde_json::Value> =
        plugins::CATALOG.iter().map(catalog_json).collect();
    all.extend(
        state
            .store
            .list_plugins(c.tenant_id)
            .into_iter()
            .map(|p| custom_json(&p)),
    );
    Ok((StatusCode::OK, Json(params.paginate(&all))))
}

/// The listing shape of a built-in catalog entry.
fn catalog_json(e: &plugins::CatalogEntry) -> serde_json::Value {
    serde_json::json!({
        "id": e.id,
        "plugin_type": kind_name(e.kind),
        "origin": "builtin",
        "served": e.served,
    })
}

/// The listing shape of a tenant-owned custom definition.
fn custom_json(p: &PluginDef) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "gts_id": p.gts_id(),
        "name": p.name,
        "description": p.description,
        "plugin_type": kind_name(p.plugin_type),
        "origin": "custom",
        "served": false,
    })
}

const fn kind_name(k: PluginKind) -> &'static str {
    match k {
        PluginKind::Auth => "auth",
        PluginKind::Guard => "guard",
        PluginKind::Transform => "transform",
    }
}

/// `GET /oagw/v1/plugins/{id}`
///
/// The identifier may be a custom definition's UUID (bare or wrapped in its
/// GTS form) or a built-in / catalog-only identifier.
pub(crate) async fn get_plugin(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    if let Some(entry) = plugins::catalog_entry(&id) {
        return Ok((StatusCode::OK, Json(catalog_json(entry))));
    }
    let uuid = plugins::instance_uuid(&id)
        .ok_or_else(|| DomainError::not_found("plugin", id.clone()))?;
    let p = state
        .store
        .get_plugin(c.tenant_id, uuid)
        .ok_or_else(|| DomainError::not_found("plugin", id.clone()))?;
    Ok((StatusCode::OK, Json(serde_json::to_value(p).unwrap_or_default())))
}

/// `GET /oagw/v1/plugins/{id}/source`
pub(crate) async fn get_plugin_source(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    // A built-in carries no source text, so it is not found here.
    let uuid = plugins::instance_uuid(&id)
        .ok_or_else(|| DomainError::not_found("plugin source", id.clone()))?;
    let p = state
        .store
        .get_plugin(c.tenant_id, uuid)
        .ok_or_else(|| DomainError::not_found("plugin source", id.clone()))?;
    Ok((StatusCode::OK, p.source_code))
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// A definition still bound to an upstream or route is a 409 naming what
/// references it.
// @cpt-begin:cpt-cf-oagw-dod-pm-in-use-delete:p1:inst-full
pub(crate) async fn delete_plugin(
    Extension(state): St,
    sec: Sec,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let c = caller(&sec);
    // A built-in is not a deletable resource.
    let id = plugins::instance_uuid(&id)
        .ok_or_else(|| DomainError::not_found("plugin", id.clone()))?;
    if state.store.get_plugin(c.tenant_id, id).is_none() {
        return Err(DomainError::not_found("plugin", id.to_string()).into());
    }
    let (upstreams, routes) = state.store.plugin_references(c.tenant_id, id);
    if !upstreams.is_empty() || !routes.is_empty() {
        return Err(DomainError::PluginInUse {
            id: id.to_string(),
            upstreams,
            routes,
        }
        .into());
    }
    state.store.delete_plugin(c.tenant_id, id)?;
    Ok(StatusCode::NO_CONTENT)
}
// @cpt-end:cpt-cf-oagw-dod-pm-in-use-delete:p1:inst-full

/// The `referenced_by` payload for a plugin, exposed for tests and callers that
/// want to inspect bindings before deleting.
#[must_use]
pub fn referenced_by(state: &OagwState, tenant: Uuid, plugin: Uuid) -> ReferencedBy {
    let (upstreams, routes) = state.store.plugin_references(tenant, plugin);
    ReferencedBy { upstreams, routes }
}

/// The built-in plugin catalog, as served.
pub(crate) async fn list_catalog() -> ApiResult<impl IntoResponse> {
    let items: Vec<serde_json::Value> = plugins::CATALOG.iter().map(catalog_json).collect();
    Ok((StatusCode::OK, Json(serde_json::json!({ "items": items }))))
}
