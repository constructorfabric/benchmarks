//! Handlers of the OAGW management surface (DESIGN §3.3 "Management API").
//!
//! Every handler derives its tenant from the authenticated
//! [`SecurityContext`] and returns [`OagwError`] on failure, so problem
//! documents carry the OAGW catalog `type` and `X-OAGW-Error-Source: gateway`.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::error::OagwError;
use crate::api::rest::body::JsonBody;
use crate::api::rest::dto::{ListRoutesQuery, PluginList, PluginSource, RouteList, UpstreamList};
use crate::api::rest::odata::{ListQuery, project};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::services::management::ManagementService;
use crate::infra::audit;
use crate::infra::storage::InMemoryStore;

/// Concrete management service backed by the in-memory store.
pub type Management = Arc<ManagementService<InMemoryStore, InMemoryStore, InMemoryStore>>;

fn tenant(context: &SecurityContext) -> Uuid {
    context.subject_tenant_id()
}

/// Serializes items for a projected list response.
fn projected_rows<T: Serialize>(items: &[T], fields: &[String]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let row = serde_json::to_value(item).unwrap_or(Value::Null);
            project(&row, fields)
        })
        .collect()
}

/// Builds the list envelope, projecting items when `$select` is present.
///
/// Without `$select` the response is the typed envelope; with it the items are
/// projected JSON objects.
fn list_response<T, L, F>(items: Vec<T>, select: Option<&[String]>, envelope: F) -> Response
where
    T: Serialize,
    L: Serialize,
    F: FnOnce(Vec<T>) -> L,
{
    match select {
        None => Json(envelope(items)).into_response(),
        Some(fields) => {
            Json(serde_json::json!({ "items": projected_rows(&items, fields) })).into_response()
        }
    }
}

// === Upstreams ===

/// `POST /oagw/v1/upstreams`.
///
/// # Errors
///
/// Propagates validation, alias-conflict and storage errors.
pub async fn create_upstream(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    JsonBody(upstream): JsonBody<Upstream>,
) -> Result<(StatusCode, Json<Upstream>), OagwError> {
    let created = service.create_upstream(tenant(&context), upstream).await?;
    audit::config_change(
        "upstream",
        "create",
        &tenant(&context).to_string(),
        &created.id.to_string(),
    );
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/upstreams`.
///
/// # Errors
///
/// Propagates storage errors.
pub async fn list_upstreams(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    ListQuery(query): ListQuery,
) -> Result<Response, OagwError> {
    let upstreams = service.list_upstreams(tenant(&context)).await?;
    Ok(list_response(upstreams, query.selected(), |items| {
        UpstreamList { items }
    }))
}

/// `GET /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent upstream.
pub async fn get_upstream(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Upstream>, OagwError> {
    Ok(Json(service.get_upstream(tenant(&context), id).await?))
}

/// `PUT /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Propagates validation, alias-immutability and conflict errors.
pub async fn replace_upstream(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    JsonBody(upstream): JsonBody<Upstream>,
) -> Result<Json<Upstream>, OagwError> {
    let replaced = service
        .replace_upstream(tenant(&context), id, upstream)
        .await?;
    audit::config_change(
        "upstream",
        "update",
        &tenant(&context).to_string(),
        &id.to_string(),
    );
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent upstream.
pub async fn delete_upstream(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    service.delete_upstream(tenant(&context), id).await?;
    audit::config_change(
        "upstream",
        "delete",
        &tenant(&context).to_string(),
        &id.to_string(),
    );
    Ok(StatusCode::NO_CONTENT)
}

// === Routes ===

/// `POST /oagw/v1/routes`.
///
/// # Errors
///
/// Propagates validation, match-conflict and storage errors.
pub async fn create_route(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    JsonBody(route): JsonBody<Route>,
) -> Result<(StatusCode, Json<Route>), OagwError> {
    let created = service.create_route(tenant(&context), route).await?;
    audit::config_change(
        "route",
        "create",
        &tenant(&context).to_string(),
        &created.id.to_string(),
    );
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/routes`.
///
/// # Errors
///
/// Propagates storage errors.
pub async fn list_routes(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Query(params): Query<ListRoutesQuery>,
    ListQuery(query): ListQuery,
) -> Result<Response, OagwError> {
    let routes = service
        .list_routes(tenant(&context), params.upstream_id)
        .await?;
    Ok(list_response(routes, query.selected(), |items| RouteList {
        items,
    }))
}

/// `GET /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent route.
pub async fn get_route(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Route>, OagwError> {
    Ok(Json(service.get_route(tenant(&context), id).await?))
}

/// `PUT /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Propagates validation and match-conflict errors; `upstream_id` is immutable.
pub async fn replace_route(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    JsonBody(route): JsonBody<Route>,
) -> Result<Json<Route>, OagwError> {
    let replaced = service.replace_route(tenant(&context), id, route).await?;
    audit::config_change(
        "route",
        "update",
        &tenant(&context).to_string(),
        &id.to_string(),
    );
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent route.
pub async fn delete_route(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    service.delete_route(tenant(&context), id).await?;
    audit::config_change(
        "route",
        "delete",
        &tenant(&context).to_string(),
        &id.to_string(),
    );
    Ok(StatusCode::NO_CONTENT)
}

// === Plugins ===

/// `POST /oagw/v1/plugins`.
///
/// # Errors
///
/// Propagates validation and name-conflict errors.
pub async fn create_plugin(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    JsonBody(plugin): JsonBody<Plugin>,
) -> Result<(StatusCode, Json<Plugin>), OagwError> {
    let created = service.create_plugin(tenant(&context), plugin).await?;
    audit::config_change(
        "plugin",
        "create",
        &tenant(&context).to_string(),
        &created.id.to_string(),
    );
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/plugins`.
///
/// # Errors
///
/// Propagates storage errors.
pub async fn list_plugins(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Query(params): Query<ListRoutesQuery>,
    ListQuery(query): ListQuery,
) -> Result<Response, OagwError> {
    let plugins = service
        .list_plugins(tenant(&context), params.plugin_type.as_deref())
        .await?;
    Ok(list_response(plugins, query.selected(), |items| {
        PluginList { items }
    }))
}

/// `GET /oagw/v1/plugins/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent plugin.
pub async fn get_plugin(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Plugin>, OagwError> {
    Ok(Json(service.get_plugin(tenant(&context), id).await?))
}

/// `DELETE /oagw/v1/plugins/{id}`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent plugin and 409 `PluginInUse` while it
/// is still referenced.
pub async fn delete_plugin(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    service.delete_plugin(tenant(&context), id).await?;
    audit::config_change(
        "plugin",
        "delete",
        &tenant(&context).to_string(),
        &id.to_string(),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source`.
///
/// # Errors
///
/// Returns 404 for a foreign or absent plugin and 400 for built-ins.
pub async fn get_plugin_source(
    Extension(service): Extension<Management>,
    Extension(context): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<PluginSource>, OagwError> {
    let source_code = service.get_plugin_source(tenant(&context), id).await?;
    Ok(Json(PluginSource {
        plugin_id: id,
        source_code,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_items_project_selected_fields() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: "api.openai.com".to_owned(),
            protocol: crate::domain::model::Protocol::Http,
            enabled: true,
            server: crate::domain::model::ServerConfig {
                endpoints: Vec::new(),
            },
            auth: crate::domain::model::AuthConfig::default(),
            headers: crate::domain::model::HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: vec!["llm".to_owned()],
            created_at: 1,
            updated_at: 1,
        };
        let rows = projected_rows(&[upstream], &["alias".to_owned(), "tags".to_owned()]);
        assert_eq!(rows[0]["alias"], "api.openai.com");
        assert_eq!(rows[0]["tags"][0], "llm");
        assert!(rows[0].get("server").is_none());
    }

    #[test]
    fn tenant_comes_from_the_security_context() {
        let context = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_type("user")
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("builds");
        assert_eq!(tenant(&context), context.subject_tenant_id());
    }
}
