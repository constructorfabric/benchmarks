//! Upstream management handlers.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, RawQuery};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ListParams, ListResponse, UpstreamDto};
use crate::api::rest::error::problem_from_domain_error;
use crate::api::rest::state::OagwState;
use crate::domain::error::DomainError;
use crate::infra::authz::resources;

/// Renders a domain error as a problem response.
pub(crate) fn problem(err: DomainError, instance: &str) -> Response {
    problem_from_domain_error(&err, instance).into_response()
}

/// The `instance` value of a management request.
pub(crate) fn instance(path: &str) -> String {
    format!("/oagw/v1{}", path)
}

/// Creates an upstream.
pub async fn create_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(request): Json<UpstreamDto>,
) -> Response {
    let instance_path = instance("/upstreams");
    let scope = match state.control_plane.authorize(&ctx, resources::UPSTREAM, "create", None).await {
        Ok(s) => s,
        Err(err) => return problem(err, &instance_path),
    };
    let tenant = ctx.subject_tenant_id();
    if !OagwState::scope_allows_public(&scope, tenant) {
        return problem(
            DomainError::PermissionDenied {
                detail: "the caller may not create upstreams for this tenant".to_string(),
            },
            &instance_path,
        );
    }
    match state.control_plane.create_upstream(tenant, request).await {
        Ok(upstream) => {
            let id = upstream.id.clone().unwrap_or_default();
            let gts_id = crate::domain::services::control_plane::ControlPlaneService::upstream_gts_id(&upstream);
            let body = axum::response::Response::builder()
                .status(StatusCode::CREATED)
                .header(
                    axum::http::header::LOCATION,
                    format!("/oagw/v1/upstreams/{id}"),
                )
                .header("X-OAGW-Error-Source", "gateway")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&json_with_gts_id(&upstream, &gts_id)).unwrap_or_default(),
                ))
                .expect("static response");
            body
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Serialises a resource with its GTS id attached.
fn json_with_gts_id(upstream: &crate::domain::dto::Upstream, gts_id: &str) -> serde_json::Value {
    let mut value = serde_json::to_value(upstream).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("gts_id".to_string(), serde_json::Value::String(gts_id.to_string()));
    }
    value
}

/// Reads an upstream by id.
pub async fn get_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/upstreams/{id}"));
    match state.control_plane.get_upstream(ctx.subject_tenant_id(), &id).await {
        Ok(upstream) => into_json(serde_json::to_value(&upstream).unwrap_or_default()),
        Err(err) => problem(err, &instance_path),
    }
}

/// Lists upstreams.
pub async fn list_upstreams(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    RawQuery(raw): RawQuery,
) -> Response {
    let instance_path = instance("/upstreams");
    let params = ListParams::from_query(&crate::api::rest::extractors::parse_query(
        raw.as_deref().unwrap_or_default(),
    ));
    match state.control_plane.list_upstreams(ctx.subject_tenant_id()).await {
        Ok(all) => {
            let all = crate::api::rest::dto::apply_filter(all, params.filter.as_deref(), |u| {
                u.alias_str().to_string()
            });
            let all = crate::api::rest::dto::apply_orderby(all, params.orderby.as_deref(), |u| {
                u.alias_str().to_string()
            });
            let (page, total) = params.paginate(all);
            let page: Vec<serde_json::Value> = page
                .iter()
                .map(|u| {
                    let gts_id =
                        crate::domain::services::control_plane::ControlPlaneService::upstream_gts_id(u);
                    json_with_gts_id(u, &gts_id)
                })
                .collect();
            into_json(serde_json::to_value(ListResponse::new(page, Some(total), None)).unwrap_or_default())
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Replaces an upstream.
pub async fn update_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(request): Json<UpstreamDto>,
) -> Response {
    let instance_path = instance(&format!("/upstreams/{id}"));
    match state
        .control_plane
        .update_upstream(ctx.subject_tenant_id(), &id, request)
        .await
    {
        Ok(upstream) => {
            let gts_id = crate::domain::services::control_plane::ControlPlaneService::upstream_gts_id(&upstream);
            into_json(json_with_gts_id(&upstream, &gts_id))
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Deletes an upstream.
pub async fn delete_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/upstreams/{id}"));
    match state.control_plane.delete_upstream(ctx.subject_tenant_id(), &id).await {
        Ok(()) => no_content(),
        Err(err) => problem(err, &instance_path),
    }
}

/// A JSON response with the OAGW error-source header.
pub(crate) fn into_json(value: serde_json::Value) -> Response {
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("X-OAGW-Error-Source", "gateway")
        .body(axum::body::Body::from(
            serde_json::to_vec(&value).unwrap_or_default(),
        ))
        .expect("static response")
}

/// A `204 No Content` response.
pub(crate) fn no_content() -> Response {
    axum::response::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("X-OAGW-Error-Source", "gateway")
        .body(axum::body::Body::empty())
        .expect("static response")
}

impl OagwState {
    /// Public helper narrowing a PDP scope to a tenant row.
    pub fn scope_allows_public(
        scope: &toolkit_security::AccessScope,
        tenant: uuid::Uuid,
    ) -> bool {
        crate::infra::authz::scope_allows(scope, tenant)
    }
}
