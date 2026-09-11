//! The five Route Management API handlers
//! (`cpt-cf-oagw-flow-route-create`/`-list`/`-get`/`-replace`/`-delete`).

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{OagwError, OagwErrorKind};
use crate::model::route::Route;
use crate::store::OagwState;

use super::ownership::resolve_owned_upstream;
use super::problem::{BareProblem, RouteApiError};
use super::query::{self, RouteListQuery};
use super::store_ops::{find_owned_route, list_owned_routes};
use super::uniqueness::collides;
use super::validate::{self, NormalizedRouteShape, violations_detail};

fn shape_to_route(
    id: Uuid,
    tenant_id: Uuid,
    upstream_id: Uuid,
    shape: NormalizedRouteShape,
) -> Route {
    Route {
        id: Some(id),
        tenant_id,
        tags: shape.tags,
        upstream_id,
        route_match: shape.route_match,
        plugins: Some(shape.plugins),
        rate_limit: shape.rate_limit,
        enabled: shape.enabled,
        priority: shape.priority,
    }
}

fn validation_error(detail: impl Into<String>) -> OagwError {
    OagwError::new(OagwErrorKind::ValidationError, detail)
}

/// `POST /oagw/v1/routes` (`cpt-cf-oagw-flow-route-create`, steps
/// `inst-route-create-request` through `inst-route-create-return`):
/// validate shape, resolve `upstream_id` ownership, check match-rule
/// uniqueness, then persist.
// @cpt-flow:cpt-cf-oagw-flow-route-create:p1
// @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-request
pub async fn create_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Route>), RouteApiError> {
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-request
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-validate-shape
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-if
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-fail
    let shape = validate::validate_route_shape(&body)
        .map_err(|violations| validation_error(violations_detail(&violations)))?;
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-fail
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-if
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-validate-shape

    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership
    let upstream_id = validate::parse_upstream_id(&body)
        .ok_or_else(|| validation_error("upstream_id is required and must be a valid UUID"))?;
    let tenant_id = ctx.subject_tenant_id();
    let owned_upstream = resolve_owned_upstream(state.store.upstreams(), tenant_id, upstream_id);
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-shape-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-if
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-fail
    owned_upstream.ok_or_else(|| {
        validation_error("upstream_id does not reference an upstream owned by the calling tenant")
    })?;
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-fail
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-if

    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness
    let candidate_http = shape.route_match.http.as_ref();
    let has_collision = collides(
        state.store.routes(),
        tenant_id,
        upstream_id,
        None,
        shape.enabled,
        candidate_http,
        shape.priority,
    );
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-ownership-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-if
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-fail
    if has_collision {
        return Err(BareProblem::conflict(
            "route match rule collides with another enabled route under this upstream",
        )
        .into());
    }
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-fail
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-if

    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-persist
    let id = Uuid::new_v4();
    let created = shape_to_route(id, tenant_id, upstream_id, shape);
    state.store.routes().insert(id, Arc::new(created.clone()));
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-persist
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-uniqueness-else
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-route-create-return
    Ok((StatusCode::CREATED, Json(created)))
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-route-create-return
}

/// `GET /oagw/v1/routes` (`cpt-cf-oagw-flow-route-list`): tenant-scoped,
/// then `$filter`/`$orderby`/`$skip`/`$top`/`$select`.
// @cpt-flow:cpt-cf-oagw-flow-route-list:p1
// @cpt-dod:cpt-cf-oagw-dod-route-list-query:p1
// @cpt-begin:cpt-cf-oagw-flow-route-list:p1:inst-route-list-request
pub async fn list_routes(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<RouteListQuery>,
) -> Result<Json<Vec<Value>>, RouteApiError> {
    // @cpt-end:cpt-cf-oagw-flow-route-list:p1:inst-route-list-request
    // @cpt-begin:cpt-cf-oagw-flow-route-list:p1:inst-route-list-query
    let tenant_id = ctx.subject_tenant_id();
    let routes = list_owned_routes(state.store.routes(), tenant_id);
    let routes = query::apply_filter(routes, params.filter.as_deref());
    let routes = query::apply_orderby(routes, params.orderby.as_deref());
    let page = query::resolve_page(params.top.as_deref(), params.skip.as_deref())
        .map_err(|detail| OagwError::new(OagwErrorKind::ValidationError, detail))?;
    let routes = query::paginate(routes, page.skip, page.top);
    // @cpt-end:cpt-cf-oagw-flow-route-list:p1:inst-route-list-query

    let select_fields = query::parse_select_fields(params.select.as_deref());
    let items = routes
        .iter()
        .map(|route| query::apply_select(route, select_fields.as_deref()))
        .collect();
    // @cpt-begin:cpt-cf-oagw-flow-route-list:p1:inst-route-list-return
    Ok(Json(items))
    // @cpt-end:cpt-cf-oagw-flow-route-list:p1:inst-route-list-return
}

/// `GET /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-get`).
// @cpt-flow:cpt-cf-oagw-flow-route-get:p1
// @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-request
pub async fn get_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> Result<Json<Route>, BareProblem> {
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-request
    // @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-resolve
    let tenant_id = ctx.subject_tenant_id();
    let resolved = Route::normalize_id_param(&raw_id)
        .and_then(|id| find_owned_route(state.store.routes(), tenant_id, id));
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-resolve
    // @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-if
    // @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-fail
    let route = resolved.ok_or_else(|| BareProblem::not_found("route not found"))?;
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-fail
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-if
    // @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-else
    // @cpt-begin:cpt-cf-oagw-flow-route-get:p1:inst-route-get-return
    Ok(Json((*route).clone()))
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-return
    // @cpt-end:cpt-cf-oagw-flow-route-get:p1:inst-route-get-else
}

/// The replace DTO's `upstream_id` immutability check
/// (`inst-route-replace-shape-if`): absent is fine (kept unchanged);
/// present-and-equal is fine (a no-op resend); present-and-different is a
/// `400 ValidationError`.
///
/// `OagwError` carries several `Option<String>` extension fields, so its
/// error-path size trips `clippy::result_large_err`; boxing it would only
/// add an allocation on a management-plane (never a hot-path) call for no
/// benefit, so this single small helper is allowed instead.
#[allow(clippy::result_large_err)]
fn check_upstream_id_unchanged(body: &Value, persisted_upstream_id: Uuid) -> Result<(), OagwError> {
    let Some(raw) = body.get("upstream_id") else {
        return Ok(());
    };
    let candidate = raw.as_str().and_then(|s| Uuid::parse_str(s).ok());
    if candidate == Some(persisted_upstream_id) {
        Ok(())
    } else {
        Err(validation_error(
            "upstream_id is immutable and may not be changed by PUT",
        ))
    }
}

/// `PUT /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-replace`): resolve
/// tenant scope, validate shape and `upstream_id` immutability, check
/// match-rule uniqueness (excluding this route), then persist.
// @cpt-flow:cpt-cf-oagw-flow-route-replace:p1
// @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-request
pub async fn replace_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Route>, RouteApiError> {
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-request
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-resolve
    let tenant_id = ctx.subject_tenant_id();
    let resolved = Route::normalize_id_param(&raw_id)
        .and_then(|id| find_owned_route(state.store.routes(), tenant_id, id));
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-resolve
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-if
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-fail
    let existing = resolved.ok_or_else(|| BareProblem::not_found("route not found"))?;
    let id = existing
        .id
        .ok_or_else(|| BareProblem::not_found("route not found"))?;
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-fail
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-if

    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-validate-shape
    check_upstream_id_unchanged(&body, existing.upstream_id)?;
    let shape = validate::validate_route_shape(&body)
        .map_err(|violations| validation_error(violations_detail(&violations)))?;
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-validate-shape
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-notfound-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-if
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-fail
    // (the two checks above realize this combined condition: shape
    // validation failure, or an attempted `upstream_id` change)
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-fail
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-if

    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness
    let candidate_http = shape.route_match.http.as_ref();
    let has_collision = collides(
        state.store.routes(),
        tenant_id,
        existing.upstream_id,
        Some(id),
        shape.enabled,
        candidate_http,
        shape.priority,
    );
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-shape-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-if
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-fail
    if has_collision {
        return Err(BareProblem::conflict(
            "route match rule collides with another enabled route under this upstream",
        )
        .into());
    }
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-fail
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-if

    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-persist
    // @cpt-begin:cpt-cf-oagw-state-route-enablement:p1:inst-route-state-to-disabled
    // @cpt-begin:cpt-cf-oagw-state-route-enablement:p1:inst-route-state-to-enabled
    let replaced = shape_to_route(id, tenant_id, existing.upstream_id, shape);
    state.store.routes().insert(id, Arc::new(replaced.clone()));
    // @cpt-end:cpt-cf-oagw-state-route-enablement:p1:inst-route-state-to-enabled
    // @cpt-end:cpt-cf-oagw-state-route-enablement:p1:inst-route-state-to-disabled
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-persist
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-uniqueness-else
    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-return
    Ok(Json(replaced))
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-route-replace-return
}

/// `DELETE /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-delete`).
// @cpt-flow:cpt-cf-oagw-flow-route-delete:p1
// @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-request
pub async fn delete_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> Result<StatusCode, BareProblem> {
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-request
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-resolve
    let tenant_id = ctx.subject_tenant_id();
    let resolved = Route::normalize_id_param(&raw_id)
        .and_then(|id| find_owned_route(state.store.routes(), tenant_id, id));
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-resolve
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-if
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-fail
    let existing = resolved.ok_or_else(|| BareProblem::not_found("route not found"))?;
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-fail
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-if

    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-else
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-persist
    if let Some(id) = existing.id {
        state.store.routes().remove(&id);
    }
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-persist
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-return
    Ok(StatusCode::NO_CONTENT)
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-return
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-route-delete-else
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn check_upstream_id_unchanged_accepts_an_absent_field() {
        assert!(check_upstream_id_unchanged(&serde_json::json!({}), Uuid::new_v4()).is_ok());
    }

    #[test]
    fn check_upstream_id_unchanged_accepts_the_same_value() {
        let id = Uuid::new_v4();
        let body = serde_json::json!({ "upstream_id": id.to_string() });
        assert!(check_upstream_id_unchanged(&body, id).is_ok());
    }

    #[test]
    fn check_upstream_id_unchanged_rejects_a_different_value() {
        let body = serde_json::json!({ "upstream_id": Uuid::new_v4().to_string() });
        assert!(check_upstream_id_unchanged(&body, Uuid::new_v4()).is_err());
    }
}
