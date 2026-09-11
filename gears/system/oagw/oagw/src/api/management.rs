//! Handlers of the management API (`/oagw/v1/...`).

use axum::Router;
use axum::extract::{FromRequest, Path, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use super::dto::{
    CatalogEntry, EnabledRequest, PluginCatalogResponse, PluginRequest, PluginResponse,
    RouteRequest, RouteResponse, UpstreamRequest, UpstreamResponse,
};
use super::{ApiState, JsonBody, json_response, tenant_of};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::infra::api::problem;

const INSTANCE: &str = "/oagw/v1";

/// Mount the management routes.
#[must_use = "the routes must be merged into the gear's router"]
pub fn routes() -> Router {
    use axum::routing::{delete, get, post};
    Router::new()
        .route("/oagw/v1/upstreams", post(create_upstream).get(list_upstreams))
        .route("/oagw/v1/upstreams/{id}", get(get_upstream).put(replace_upstream))
        .route("/oagw/v1/upstreams/{id}/enabled", post(set_upstream_enabled))
        .route("/oagw/v1/upstreams/{id}", delete(delete_upstream))
        .route("/oagw/v1/routes", post(create_route).get(list_routes))
        .route("/oagw/v1/routes/{id}", get(get_route).put(replace_route))
        .route("/oagw/v1/routes/{id}/enabled", post(set_route_enabled))
        .route("/oagw/v1/routes/{id}", delete(delete_route))
        .route("/oagw/v1/plugins", get(get_plugins).post(create_plugin))
        .route("/oagw/v1/plugins/{id}", get(get_plugin).delete(delete_plugin))
        .route("/oagw/v1/plugins/{id}/source", get(get_plugin_source))
}

fn fail(error: &DomainError, instance: &str) -> Response {
    problem::problem_response(error, &meta_for(error), instance)
}

fn meta_for(error: &DomainError) -> crate::domain::ProblemMeta {
    crate::domain::ProblemMeta::new().with_code(error.problem_type())
}

/// Mount the error-source layer on the response.
fn ok(status: StatusCode, body: &impl serde::Serialize) -> Response {
    let mut response = json_response(status, body);
    problem::stamp_gateway_source(&mut response);
    response
}

// ---- upstreams ---------------------------------------------------------

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let JsonBody(body): JsonBody<UpstreamRequest> = match JsonBody::from_request(request, &()).await
    {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    let upstream = Upstream {
        id: Uuid::now_v7(),
        tenant_id: tenant,
        alias: String::new(),
        enabled: body.enabled.unwrap_or(true),
        protocol: body.protocol.clone(),
        server: body.server.clone(),
        auth: body.auth.clone(),
        headers: body.headers.clone(),
        plugins: body.plugins.clone(),
        rate_limit: body.rate_limit.clone(),
        cors: body.cors.clone(),
        tags: body.tags.clone().unwrap_or_default(),
    };
    match state.control.create_upstream(upstream, body.alias.clone()) {
        Ok(created) => ok(StatusCode::CREATED, &UpstreamResponse::from(created)),
        Err(error) => fail(&error, INSTANCE),
    }
}

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let items: Vec<UpstreamResponse> = state
        .control
        .list_upstreams(Some(tenant))
        .into_iter()
        .map(UpstreamResponse::from)
        .collect();
    ok(StatusCode::OK, &items)
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get_upstream(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    match state.control.get_upstream(tenant, id) {
        Ok(found) => ok(StatusCode::OK, &UpstreamResponse::from(found)),
        Err(error) => fail(&error, &format!("{INSTANCE}/upstreams/{id}")),
    }
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn replace_upstream(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/upstreams/{id}");
    let JsonBody(body): JsonBody<UpstreamRequest> = match JsonBody::from_request(request, &()).await
    {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    let upstream = Upstream {
        id,
        tenant_id: tenant,
        alias: String::new(),
        enabled: body.enabled.unwrap_or(true),
        protocol: body.protocol.clone(),
        server: body.server.clone(),
        auth: body.auth.clone(),
        headers: body.headers.clone(),
        plugins: body.plugins.clone(),
        rate_limit: body.rate_limit.clone(),
        cors: body.cors.clone(),
        tags: body.tags.clone().unwrap_or_default(),
    };
    match state.control.replace_upstream(upstream, body.alias.clone()) {
        Ok(replaced) => ok(StatusCode::OK, &UpstreamResponse::from(replaced)),
        Err(error) => fail(&error, &instance),
    }
}

/// `POST /oagw/v1/upstreams/{id}/enabled`
pub async fn set_upstream_enabled(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/upstreams/{id}/enabled");
    let JsonBody(body): JsonBody<EnabledRequest> = match JsonBody::from_request(request, &()).await
    {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    match state.control.set_upstream_enabled(tenant, id, body.enabled) {
        Ok(found) => ok(StatusCode::OK, &UpstreamResponse::from(found)),
        Err(error) => fail(&error, &instance),
    }
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete_upstream(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/upstreams/{id}");
    match state.control.delete_upstream(tenant, id) {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            problem::stamp_gateway_source(&mut response);
            response
        }
        Err(error) => fail(&error, &instance),
    }
}

// ---- routes ------------------------------------------------------------

/// `POST /oagw/v1/routes`
pub async fn create_route(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let JsonBody(body): JsonBody<RouteRequest> = match JsonBody::from_request(request, &()).await {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    let route = Route {
        id: Uuid::now_v7(),
        tenant_id: tenant,
        upstream_id: body.upstream_id,
        enabled: body.enabled.unwrap_or(true),
        route_match: body.route_match.clone(),
        plugins: body.plugins.clone(),
        rate_limit: body.rate_limit.clone(),
        cors: body.cors.clone(),
        tags: body.tags.clone().unwrap_or_default(),
    };
    match state.control.create_route(route) {
        Ok(created) => ok(StatusCode::CREATED, &RouteResponse::from(created)),
        Err(error) => fail(&error, INSTANCE),
    }
}

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let items: Vec<RouteResponse> = state
        .control
        .list_routes(Some(tenant))
        .into_iter()
        .map(RouteResponse::from)
        .collect();
    ok(StatusCode::OK, &items)
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get_route(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    match state.control.get_route(tenant, id) {
        Ok(found) => ok(StatusCode::OK, &RouteResponse::from(found)),
        Err(error) => fail(&error, &format!("{INSTANCE}/routes/{id}")),
    }
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn replace_route(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/routes/{id}");
    let JsonBody(body): JsonBody<RouteRequest> = match JsonBody::from_request(request, &()).await {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    let route = Route {
        id,
        tenant_id: tenant,
        upstream_id: body.upstream_id,
        enabled: body.enabled.unwrap_or(true),
        route_match: body.route_match.clone(),
        plugins: body.plugins.clone(),
        rate_limit: body.rate_limit.clone(),
        cors: body.cors.clone(),
        tags: body.tags.clone().unwrap_or_default(),
    };
    match state.control.replace_route(route) {
        Ok(replaced) => ok(StatusCode::OK, &RouteResponse::from(replaced)),
        Err(error) => fail(&error, &instance),
    }
}

/// `POST /oagw/v1/routes/{id}/enabled`
pub async fn set_route_enabled(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/routes/{id}/enabled");
    let JsonBody(body): JsonBody<EnabledRequest> = match JsonBody::from_request(request, &()).await
    {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    match state.control.set_route_enabled(tenant, id, body.enabled) {
        Ok(found) => ok(StatusCode::OK, &RouteResponse::from(found)),
        Err(error) => fail(&error, &instance),
    }
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete_route(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/routes/{id}");
    match state.control.delete_route(tenant, id) {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            problem::stamp_gateway_source(&mut response);
            response
        }
        Err(error) => fail(&error, &instance),
    }
}

// ---- plugins -----------------------------------------------------------

/// `GET /oagw/v1/plugins`
pub async fn get_plugins(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let (auth, guards, transforms) = state.data.registries();
    let builtins: Vec<CatalogEntry> = gts_helpers::builtin_catalogue()
        .into_iter()
        .map(|(id, kind)| CatalogEntry {
            id,
            kind,
            resolvable: match kind {
                "auth" => auth.resolvable(id),
                "guard" => guards.resolvable(id),
                _ => transforms.resolvable(id),
            },
        })
        .collect();
    let plugins: Vec<PluginResponse> = state
        .control
        .list_plugins(Some(tenant))
        .into_iter()
        .map(|plugin| {
            let refs = state.control.referencing_resources(tenant, plugin.id);
            PluginResponse::from_plugin(plugin, refs)
        })
        .collect();
    ok(
        StatusCode::OK,
        &PluginCatalogResponse { builtins, plugins },
    )
}

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let JsonBody(body): JsonBody<PluginRequest> = match JsonBody::from_request(request, &()).await {
        Ok(body) => body,
        Err(rejection) => return rejection,
    };
    let plugin = Plugin {
        id: Uuid::now_v7(),
        tenant_id: tenant,
        name: body.name.clone(),
        description: body.description.clone(),
        plugin_type: body.plugin_type.clone(),
        phases: body.phases.clone().unwrap_or_default(),
        config_schema: body.config_schema.clone(),
        source_code: body.source_code.clone(),
    };
    match state.control.create_plugin(plugin) {
        Ok(created) => {
            let refs = state.control.referencing_resources(tenant, created.id);
            ok(StatusCode::CREATED, &PluginResponse::from_plugin(created, refs))
        }
        Err(error) => fail(&error, INSTANCE),
    }
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get_plugin(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    match state.control.get_plugin(tenant, id) {
        Ok(found) => {
            let refs = state.control.referencing_resources(tenant, found.id);
            ok(StatusCode::OK, &PluginResponse::from_plugin(found, refs))
        }
        Err(error) => fail(&error, &format!("{INSTANCE}/plugins/{id}")),
    }
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete_plugin(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/plugins/{id}");
    match state.control.delete_plugin(tenant, id) {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            problem::stamp_gateway_source(&mut response);
            response
        }
        Err(error) => fail(&error, &instance),
    }
}

/// `GET /oagw/v1/plugins/{id}/source`
pub async fn get_plugin_source(
    axum::Extension(state): axum::Extension<ApiState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Response {
    let tenant = tenant_of(&request);
    let instance = format!("{INSTANCE}/plugins/{id}/source");
    match state.control.get_plugin(tenant, id) {
        Ok(found) => {
            let Some(source) = found.source_code else {
                let error = DomainError::Validation(format!(
                    "plugin {} has no source code",
                    found.id
                ));
                return fail(&error, &instance);
            };
            ok(
                StatusCode::OK,
                &SourceResponse { source },
            )
        }
        Err(error) => fail(&error, &instance),
    }
}

/// Body of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, serde::Serialize)]
pub struct SourceResponse {
    /// Starlark source of the plugin.
    pub source: String,
}
