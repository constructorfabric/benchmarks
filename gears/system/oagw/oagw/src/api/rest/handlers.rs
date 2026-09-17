//! REST handlers for the OAGW control plane and the proxy surface.
//!
//! Management handlers implement
//! `cpt-cf-oagw-flow-control-plane-manage-{upstreams,routes,plugins}`: every
//! operation is authz-gated before any persistence (`inst-authz-gate`), the
//! caller is mapped to its tenant scope (`inst-tenant-scope`), the request
//! body is validated (`inst-schema-validate`), the write is persisted by the
//! domain repository, and every write invalidates the effective-config L1
//! caches. The proxy handler relays ingress requests to the pingora
//! data-plane bridge (feature-data-plane).

// DoD traceability (`cpt-cf-oagw-dod-control-plane-*` — to_code markers).
// @cpt-dod:cpt-cf-oagw-dod-control-plane-upstream-crud:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-route-crud:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-plugin-crud:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-hierarchy:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-cache-invalidation:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-persistence:p2
// @cpt-dod:cpt-cf-oagw-dod-control-plane-test-harness:p2
use std::sync::Arc;

use authz_resolver_sdk::pep::{EnforcerError, PolicyEnforcer, ResourceType};
use axum::Json;
use axum::extract::{Extension, Path, Request};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use toolkit::api::canonical_prelude::*;
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use crate::api::rest::{ProxyPort, dto};
use crate::domain::error::DomainError;
use crate::domain::models::Plugin;
use crate::domain::repository::ControlPlaneService;
use crate::infra::proxy::relay::{RelayError, RelayIdentity, RelayRequest, relay_request};

/// PEP resource types for the management surface. The tenant-scope and
/// resource-id properties are declared so the PDP's row-level constraints
/// compile (fail-open for in-scope tenants, fail-closed otherwise).
const UPSTREAM_RESOURCE: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.oagw.upstream.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);
const ROUTE_RESOURCE: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.oagw.route.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);
const PLUGIN_RESOURCE: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.oagw.plugin.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

/// PEP gate for management operations (`inst-authz-gate`).
async fn authorize(
    enforcer: &PolicyEnforcer,
    security: &SecurityContext,
    resource: &ResourceType,
    action: &str,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-gate
    match enforcer
        .access_scope(security, resource, action, None)
        .await
    {
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-gate
        // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-allowed
        Ok(_) => Ok(()),
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-allowed
        Err(e) => Err(pep_domain_error(e)),
    }
}

/// Maps a PEP error onto the domain error ladder.
fn pep_domain_error(e: EnforcerError) -> DomainError {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-forbidden
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-return
    match e {
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-forbidden
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-return
        EnforcerError::Denied { deny_reason } => {
            // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-error
            let detail = deny_reason
                // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-error
                .as_ref()
                .and_then(|r| r.details.clone())
                .unwrap_or_else(|| "authorization denied".to_owned());
            DomainError::PepDenied(detail)
        }
        // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-error-fallback
        other => DomainError::Internal(format!("PEP evaluation failed: {other}")),
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-authz-error-fallback
    }
}

/// Tenant-scope gate (`inst-tenant-scope`).
fn authorize_tenant(
    control: &ControlPlaneService,
    security: &SecurityContext,
    resource: &str,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-tenant-scope
    control.assert_tenant(&security.subject_tenant_id().to_string(), resource)
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-tenant-scope
}

// ---------------------------------------------------------------------------
// Upstreams (`cpt-cf-oagw-flow-control-plane-manage-upstreams`)
// ---------------------------------------------------------------------------

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Json<Vec<dto::UpstreamDto>>> {
    authorize(&enforcer, &ctx, &UPSTREAM_RESOURCE, "list").await?;
    authorize_tenant(&control, &ctx, "upstreams")?;
    Ok(Json(
        control
            .list_upstreams()
            .iter()
            .map(dto::UpstreamDto::from_domain)
            .collect(),
    ))
}

/// `GET /oagw/v1/upstreams/{alias}`
pub async fn get_upstream(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<Json<dto::UpstreamDto>> {
    authorize(&enforcer, &ctx, &UPSTREAM_RESOURCE, "read").await?;
    authorize_tenant(&control, &ctx, "upstreams")?;
    let upstream = control
        .get_upstream(&alias)
        .ok_or_else(|| DomainError::UpstreamNotFound(alias.clone()))?;
    Ok(Json(dto::UpstreamDto::from_domain(&upstream)))
}

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(req): Json<dto::UpstreamRequest>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-upstream-request
    authorize(&enforcer, &ctx, &UPSTREAM_RESOURCE, "create").await?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-upstream-request
    authorize_tenant(&control, &ctx, "upstreams")?;
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-validate
    let upstream = req.into_domain(None)?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-validate
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-invalid
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-400
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-valid
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-alias-rules
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-persist-upstream
    control.upsert_upstream(upstream.clone())?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-invalid
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-400
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-schema-valid
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-alias-rules
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-persist-upstream
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-binding-conflict
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-binding-409
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-write-ok
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-invalidate
    let dto = dto::UpstreamDto::from_domain(&upstream);
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-binding-conflict
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-binding-409
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-write-ok
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-invalidate
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-return-ok
    Ok(created_json(dto, &uri, &upstream.alias))
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-upstreams:ph-1:inst-return-ok
}

/// `PUT /oagw/v1/upstreams/{alias}`
pub async fn update_upstream(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    Json(req): Json<dto::UpstreamRequest>,
) -> ApiResult<Json<dto::UpstreamDto>> {
    authorize(&enforcer, &ctx, &UPSTREAM_RESOURCE, "update").await?;
    authorize_tenant(&control, &ctx, "upstreams")?;
    let existing = control
        .get_upstream(&alias)
        .ok_or_else(|| DomainError::UpstreamNotFound(alias.clone()))?;
    let upstream = req.into_domain(Some(&existing))?;
    control.update_upstream(upstream.clone())?;
    Ok(Json(dto::UpstreamDto::from_domain(&upstream)))
}

/// `DELETE /oagw/v1/upstreams/{alias}`
pub async fn delete_upstream(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<impl IntoResponse> {
    authorize(&enforcer, &ctx, &UPSTREAM_RESOURCE, "delete").await?;
    authorize_tenant(&control, &ctx, "upstreams")?;
    control.delete_upstream(&alias)?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Routes (`cpt-cf-oagw-flow-control-plane-manage-routes`)
// ---------------------------------------------------------------------------

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Json<Vec<dto::RouteDto>>> {
    authorize(&enforcer, &ctx, &ROUTE_RESOURCE, "list").await?;
    authorize_tenant(&control, &ctx, "routes")?;
    Ok(Json(
        control
            .list_routes()
            .iter()
            .map(dto::RouteDto::from_domain)
            .collect(),
    ))
}

/// `GET /oagw/v1/routes/{alias}`
pub async fn get_route(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<Json<dto::RouteDto>> {
    authorize(&enforcer, &ctx, &ROUTE_RESOURCE, "read").await?;
    authorize_tenant(&control, &ctx, "routes")?;
    let route = control
        .get_route(&alias)
        .ok_or_else(|| DomainError::RouteNotFound(alias.clone()))?;
    Ok(Json(dto::RouteDto::from_domain(&route)))
}

/// `POST /oagw/v1/routes`
pub async fn create_route(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(req): Json<dto::RouteRequest>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-request
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-authz
    authorize(&enforcer, &ctx, &ROUTE_RESOURCE, "create").await?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-request
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-authz
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-forbidden
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-forbidden-return
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-allowed
    authorize_tenant(&control, &ctx, "routes")?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-forbidden
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-forbidden-return
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-allowed
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-tenant
    let route = req.into_domain(None)?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-tenant
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema-invalid
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-400
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema-valid
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-upstream-check
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-disabled-upstream
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-409
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-valid-upstreams
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-persist-route
    control.upsert_route(route.clone())?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema-invalid
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-400
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-schema-valid
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-upstream-check
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-disabled-upstream
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-409
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-valid-upstreams
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-persist-route
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-invalidate
    let dto = dto::RouteDto::from_domain(&route);
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-invalidate
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-return-ok
    Ok(created_json(dto, &uri, &route.alias))
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-routes:ph-1:inst-route-return-ok
}

/// `PUT /oagw/v1/routes/{alias}`
pub async fn update_route(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    Json(req): Json<dto::RouteRequest>,
) -> ApiResult<Json<dto::RouteDto>> {
    authorize(&enforcer, &ctx, &ROUTE_RESOURCE, "update").await?;
    authorize_tenant(&control, &ctx, "routes")?;
    if control.get_route(&alias).is_none() {
        return Err(DomainError::RouteNotFound(alias).into());
    }
    let route = req.into_domain(Some(&alias))?;
    control.update_route(route.clone())?;
    Ok(Json(dto::RouteDto::from_domain(&route)))
}

/// `DELETE /oagw/v1/routes/{alias}`
pub async fn delete_route(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<impl IntoResponse> {
    authorize(&enforcer, &ctx, &ROUTE_RESOURCE, "delete").await?;
    authorize_tenant(&control, &ctx, "routes")?;
    control.delete_route(&alias)?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Plugins (`cpt-cf-oagw-flow-control-plane-manage-plugins`)
// ---------------------------------------------------------------------------

fn plugin_from(req: dto::PluginRequest, path_alias: Option<&str>) -> Result<Plugin, DomainError> {
    // The alias is authoritative from the path for updates; a non-empty body
    // alias that disagrees is rejected instead of silently re-targeting.
    let alias = match path_alias {
        Some(pa) if !req.alias.is_empty() && req.alias != pa => {
            return Err(DomainError::validation(format!(
                "body alias `{}` conflicts with path alias `{pa}`",
                req.alias
            )));
        }
        Some(pa) => pa.to_owned(),
        None => req.alias,
    };
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema
    let kind = crate::domain::models::PluginKind::parse(&req.kind).ok_or_else(|| {
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema
        DomainError::validation(format!("unknown plugin kind `{}`", req.kind))
    })?;
    Ok(Plugin {
        alias,
        kind,
        enabled: req.enabled,
        config: req.config,
    })
}

/// `GET /oagw/v1/plugins`
pub async fn list_plugins(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Json<Vec<dto::PluginDto>>> {
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "list").await?;
    authorize_tenant(&control, &ctx, "plugins")?;
    Ok(Json(
        control
            .list_plugins()
            .iter()
            .map(dto::PluginDto::from_domain)
            .collect(),
    ))
}

/// `GET /oagw/v1/plugins/{alias}`
pub async fn get_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<Json<dto::PluginDto>> {
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "read").await?;
    authorize_tenant(&control, &ctx, "plugins")?;
    let plugin = control
        .get_plugin(&alias)
        .ok_or_else(|| DomainError::PluginNotFound(alias.clone()))?;
    Ok(Json(dto::PluginDto::from_domain(&plugin)))
}

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(req): Json<dto::PluginRequest>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-request
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-authz
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "create").await?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-request
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-authz
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-forbidden
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-forbidden-return
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-allowed
    authorize_tenant(&control, &ctx, "plugins")?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-forbidden
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-forbidden-return
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-allowed
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-tenant
    let plugin = plugin_from(req, None)?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-tenant
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema-invalid
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-400
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema-valid
    // upsert + invalidation inside the repository
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema-invalid
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-400
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-schema-valid
    control.upsert_plugin(plugin.clone())?;
    let dto = dto::PluginDto::from_domain(&plugin);
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-return-ok
    Ok(created_json(dto, &uri, &plugin.alias))
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-return-ok
}

/// `PUT /oagw/v1/plugins/{alias}`
pub async fn update_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    Json(req): Json<dto::PluginRequest>,
) -> ApiResult<Json<dto::PluginDto>> {
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "update").await?;
    authorize_tenant(&control, &ctx, "plugins")?;
    if control.get_plugin(&alias).is_none() {
        return Err(DomainError::PluginNotFound(alias).into());
    }
    let plugin = plugin_from(req, Some(&alias))?;
    control.update_plugin(plugin.clone())?;
    Ok(Json(dto::PluginDto::from_domain(&plugin)))
}

/// `DELETE /oagw/v1/plugins/{alias}`
pub async fn delete_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
) -> ApiResult<impl IntoResponse> {
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "delete").await?;
    authorize_tenant(&control, &ctx, "plugins")?;
    control.delete_plugin(&alias)?;
    Ok(no_content())
}

fn binding_targets(
    req: &dto::BindRequest,
) -> Result<(Option<String>, Option<String>), DomainError> {
    match (&req.upstream, &req.route) {
        (Some(_), Some(_)) | (None, None) => Err(DomainError::validation(
            "bind requires exactly one of `upstream` or `route`",
        )),
        other => Ok((other.0.clone(), other.1.clone())),
    }
}

/// `POST /oagw/v1/plugins/{alias}/bind`
pub async fn bind_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    Json(req): Json<dto::BindRequest>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-request
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "update").await?;
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-request
    authorize_tenant(&control, &ctx, "plugins")?;
    let (upstream, route) = binding_targets(&req)?;
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-bind
    if let Some(ua) = upstream {
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-bind
        control.bind_plugin_to_upstream(&ua, &alias)?;
    } else if let Some(ra) = route {
        control.bind_plugin_to_route(&ra, &alias)?;
    }
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-in-use
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-409
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-bind-row
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-invalidate
    Ok(no_content())
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-in-use
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-409
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-bind-row
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-invalidate
}

/// `POST /oagw/v1/plugins/{alias}/unbind`
pub async fn unbind_plugin(
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Extension(enforcer): Extension<Arc<PolicyEnforcer>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    Json(req): Json<dto::BindRequest>,
) -> ApiResult<impl IntoResponse> {
    authorize(&enforcer, &ctx, &PLUGIN_RESOURCE, "update").await?;
    authorize_tenant(&control, &ctx, "plugins")?;
    let (upstream, route) = binding_targets(&req)?;
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-unbind
    if let Some(ua) = upstream {
        // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-unbind
        control.unbind_plugin_from_upstream(&ua, &alias)?;
    } else if let Some(ra) = route {
        control.unbind_plugin_from_route(&ra, &alias)?;
    }
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-still-bound
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-unbind-409
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-delete-row
    // @cpt-begin:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-invalidate
    Ok(no_content())
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-still-bound
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-unbind-409
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-delete-row
    // @cpt-end:cpt-cf-oagw-flow-control-plane-manage-plugins:ph-1:inst-plugin-invalidate
}

// ---------------------------------------------------------------------------
// Proxy surface (feature-data-plane transport stage)
// ---------------------------------------------------------------------------

/// Extracts the route alias from the request path; host-prefix agnostic
/// (`…/proxy/{alias}/{*path}`).
fn extract_alias(path: &str) -> Option<String> {
    path.split("/proxy/")
        .nth(1)
        .and_then(|p| p.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Maps a gateway-sourced status onto the gear's problem types so the
/// published `type` agrees with the status code (no status/type mismatch).
fn relay_type(status: u16) -> &'static str {
    use crate::infra::proxy::error_types as et;
    match status {
        400 => et::VALIDATION,
        404 => et::ROUTE_NOT_FOUND,
        413 => et::PAYLOAD_TOO_LARGE,
        429 => et::RATE_LIMIT,
        502 => et::DOWNSTREAM,
        503 => et::LINK_UNAVAILABLE,
        _ => et::INTERNAL,
    }
}

/// Builds an RFC 9457 problem response for a gateway-sourced failure.
fn relay_problem(status: u16, title: &str, detail: String) -> Response {
    let body = serde_json::json!({
        "type": relay_type(status),
        "title": title,
        "status": status,
        "detail": detail,
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    match Response::builder()
        .status(status)
        .header("content-type", "application/problem+json")
        .header("X-OAGW-Error-Source", "gateway")
        .body(axum::body::Body::from(bytes))
    {
        Ok(resp) => resp,
        // Infallible fallback: fixed status, no headers.
        Err(_) => {
            let mut resp = Response::new(axum::body::Body::from(b"gateway relay failure".to_vec()));
            *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            resp
        }
    }
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path}` — relay into the data-plane
/// bridge. The bridge port is read from the shared [`ProxyPort`] cell, written
/// once the pingora listener is bound (see `gear.rs::serve`).
pub async fn proxy_relay(
    Extension(ctx): Extension<SecurityContext>,
    Extension(port): Extension<ProxyPort>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some(alias) = extract_alias(&path) else {
        return relay_problem(
            400,
            "Bad Request",
            "cannot resolve proxy alias from path".to_owned(),
        );
    };

    let bridge_port = port.port();
    if bridge_port == 0 {
        return relay_problem(
            503,
            "Data plane not ready",
            "the OAGW data-plane bridge is not running".to_owned(),
        );
    }

    // Forward the caller's headers; the relay filters transport and internal
    // header names itself.
    let mut headers = HeaderMap::new();
    headers.extend(
        request
            .headers()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let identity = RelayIdentity {
        alias,
        subject_id: ctx.subject_id(),
        tenant_id: ctx.subject_tenant_id(),
        bearer: ctx.bearer_token().map(|s| s.expose_secret().to_owned()),
        scopes: ctx.token_scopes().to_vec(),
        // Stamp the per-bridge relay secret so the pingora gate will accept
        // this internal request (unset until the bridge binds).
        relay_secret: port.relay_secret().unwrap_or_default(),
    };

    let uri = request
        .uri()
        .path_and_query()
        .map(|pq| {
            if let Some(q) = pq.query() {
                format!("{}?{}", pq.path(), q)
            } else {
                pq.path().to_owned()
            }
        })
        .unwrap_or(path);

    let relay_req = RelayRequest {
        method: request.method().as_str().to_owned(),
        uri,
        headers,
        identity,
        body: request.into_body(),
    };

    match relay_request(bridge_port, relay_req).await {
        Ok(resp) => resp,
        Err(RelayError::BodyTooLarge(limit)) => relay_problem(
            413,
            "Payload Too Large",
            format!("request body exceeds the {limit}-byte gateway cap"),
        ),
        Err(e) => relay_problem(502, "Bad Gateway", format!("relay failure: {e}")),
    }
}
