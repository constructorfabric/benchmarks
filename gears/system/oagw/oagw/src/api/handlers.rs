//! Management-surface handlers.
//!
//! Each handler resolves the caller's tenant chain, hands it to the store and maps the
//! store's error into the canonical catalog. Nothing here touches credential material.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::dto::{PluginDto, PluginList, RouteDto, UpstreamDto, UpstreamList};
use crate::store::list::ListQuery as ParsedQuery;
use crate::error::OagwError;
use crate::proxy::ProxyService;
use crate::store::TenantChain;

/// Concrete service alias for the handlers.
pub(crate) type ConcreteService = ProxyService;

/// The `OData` system query options the management surface accepts.
///
/// The values are kept as raw strings on purpose: the store's own list-options parser
/// owns the validation and the error wording, so the handler only has to hand the
/// options back verbatim.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct ListParams {
    /// `$filter`
    #[serde(rename = "$filter")]
    filter: Option<String>,
    /// `$select`
    #[serde(rename = "$select")]
    select: Option<String>,
    /// `$orderby`
    #[serde(rename = "$orderby")]
    orderby: Option<String>,
    /// `$top`
    #[serde(rename = "$top")]
    top: Option<String>,
    /// `$skip`
    #[serde(rename = "$skip")]
    skip: Option<String>,
}

impl ListParams {
    /// Rebuilds the query string the extractor already decoded, so the store's own
    /// list-options parser can reject what it does not support.
    #[must_use]
    pub fn to_query_string(&self) -> String {
        let mut query = String::new();
        for (name, value) in [
            ("$filter", self.filter.as_deref()),
            ("$select", self.select.as_deref()),
            ("$orderby", self.orderby.as_deref()),
            ("$top", self.top.as_deref()),
            ("$skip", self.skip.as_deref()),
        ] {
            if let Some(value) = value {
                if !query.is_empty() {
                    query.push('&');
                }
                query.push_str(name);
                query.push('=');
                query.push_str(value);
            }
        }
        query
    }

    /// Parses the extracted parameters.
    ///
    /// # Errors
    ///
    /// Returns a validation error when an option is malformed.
    pub fn parse(&self) -> Result<ParsedQuery, OagwError> {
        ParsedQuery::parse(&self.to_query_string())
    }
}

/// Lists the upstreams visible to the caller.
///
/// # Errors
///
/// Returns a validation error when the query options are malformed.
pub async fn list_upstreams(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let query = params.parse()?;
    let chain = chain(&svc, &ctx).await?;
    let total = svc.store().list_upstreams(&chain).len();
    let values: Vec<Value> = svc
        .store()
        .list_upstreams(&chain)
        .iter()
        .map(|u| serde_json::to_value(UpstreamDto::from_entity(u)).unwrap_or(Value::Null))
        .collect();
    let items = query.filter(query.apply(values));
    Ok(ok_json(UpstreamList {
        items: project(items, params.select.as_deref()),
        total: total as u64,
    }))
}

/// Creates an upstream.
///
/// # Errors
///
/// Returns a validation error for an invalid definition and a conflict error when the
/// alias is already taken in the tenant.
pub async fn create_upstream(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<UpstreamDto>,
) -> ApiResult<impl IntoResponse> {
    let _chain = chain(&svc, &ctx).await?;
    let mut entity = body.into_entity(String::new(), ctx.subject_tenant_id());
    if entity.alias.is_empty() {
        let endpoints: Vec<(String, u16, &str)> = entity
            .server
            .endpoints
            .iter()
            .map(|e| (e.host.clone(), e.port, e.scheme.as_str()))
            .collect();
        entity.alias = crate::domain::alias::derive(&endpoints).unwrap_or_default();
    }
    entity.validate(svc.config())?;
    let created = svc
        .store()
        .insert_upstream(entity, ctx.subject_tenant_id())?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(UpstreamDto::from_entity(&created)).unwrap_or(Value::Null)),
    )
        .into_response())
}

/// Reads one upstream.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's chain.
pub async fn get_upstream(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let entity = svc
        .store()
        .get_upstream(&id, &chain)
        .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "upstream not found"))?;
    Ok(ok_json(UpstreamDto::from_entity(&entity)))
}

/// Replaces an upstream's configuration.
///
/// # Errors
///
/// Returns not-found, validation and conflict errors as the store reports them.
pub async fn replace_upstream(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<UpstreamDto>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let existing = svc
        .store()
        .get_upstream(&id, &chain)
        .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "upstream not found"))?;
    let mut entity = body.into_entity(existing.id.clone(), existing.tenant_id);
    entity.alias.clone_from(&existing.alias);
    entity.validate(svc.config())?;
    let replaced = svc.store().replace_upstream(&id, entity, &chain)?;
    Ok(ok_json(UpstreamDto::from_entity(&replaced)))
}

/// Deletes an upstream.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's chain.
pub async fn delete_upstream(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    svc.store().delete_upstream(&id, &chain)?;
    Ok(no_content())
}

/// Lists the routes visible to the caller.
///
/// # Errors
///
/// Returns a validation error when the query options are malformed.
pub async fn list_routes(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let query = params.parse()?;
    let chain = chain(&svc, &ctx).await?;
    let total = svc.store().list_routes(&chain).len();
    let values: Vec<Value> = svc
        .store()
        .list_routes(&chain)
        .iter()
        .map(|r| serde_json::to_value(RouteDto::from_entity(r)).unwrap_or(Value::Null))
        .collect();
    let items = query.filter(query.apply(values));
    Ok(ok_json(UpstreamList {
        items: project(items, params.select.as_deref()),
        total: total as u64,
    }))
}

/// Creates a route.
///
/// # Errors
///
/// Returns a validation error for an invalid definition, a not-found error when the
/// upstream is unknown and a conflict error when an enabled route already matches.
pub async fn create_route(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<RouteDto>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let upstream = svc
        .store()
        .get_upstream(&body.upstream_id, &chain)
        .ok_or_else(|| {
            OagwError::new(crate::error::ErrorKind::RouteNotFound, "upstream not found")
        })?;
    let entity = body.into_entity(String::new(), ctx.subject_tenant_id(), upstream.id);
    entity.validate()?;
    let created = svc.store().insert_route(entity, &chain)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(RouteDto::from_entity(&created)).unwrap_or(Value::Null)),
    )
        .into_response())
}

/// Reads one route.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's chain.
pub async fn get_route(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let entity = svc
        .store()
        .get_route(&id, &chain)
        .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "route not found"))?;
    Ok(ok_json(RouteDto::from_entity(&entity)))
}

/// Replaces a route's configuration.
///
/// # Errors
///
/// Returns not-found, validation and conflict errors as the store reports them.
pub async fn replace_route(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<RouteDto>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let existing = svc
        .store()
        .get_route(&id, &chain)
        .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "route not found"))?;
    let entity = body.into_entity(existing.id.clone(), existing.tenant_id, existing.upstream_id);
    entity.validate()?;
    let replaced = svc.store().replace_route(&id, entity, &chain)?;
    Ok(ok_json(RouteDto::from_entity(&replaced)))
}

/// Deletes a route.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's chain.
pub async fn delete_route(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    svc.store().delete_route(&id, &chain)?;
    Ok(no_content())
}

/// Lists the plugin definitions visible to the caller.
///
/// # Errors
///
/// Returns a validation error when the query options are malformed.
pub async fn list_plugins(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<impl IntoResponse> {
    let query = params.parse()?;
    let chain = chain(&svc, &ctx).await?;
    let total = svc.store().list_plugins(&chain).len();
    let values: Vec<Value> = svc
        .store()
        .list_plugins(&chain)
        .iter()
        .map(|p| serde_json::to_value(PluginDto::from_entity(p)).unwrap_or(Value::Null))
        .collect();
    let items = query.apply(values);
    Ok(ok_json(PluginList {
        items: project(items, params.select.as_deref()),
        total: total as u64,
    }))
}

/// Creates a plugin definition.
///
/// # Errors
///
/// Returns a validation error when the definition has no name.
pub async fn create_plugin(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<PluginDto>,
) -> ApiResult<impl IntoResponse> {
    let _chain = chain(&svc, &ctx).await?;
    let created = svc
        .store()
        .insert_plugin(body.into_entity(String::new(), ctx.subject_tenant_id()), ctx.subject_tenant_id())?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(PluginDto::from_entity(&created)).unwrap_or(Value::Null)),
    )
        .into_response())
}

/// Reads one plugin definition.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's chain.
pub async fn get_plugin(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    let entity = svc
        .store()
        .get_plugin(&id, &chain)
        .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "plugin not found"))?;
    Ok(ok_json(PluginDto::from_entity(&entity)))
}

/// Deletes a plugin definition.
///
/// # Errors
///
/// Returns a not-found error when the identifier is unknown and `PluginInUse` when an
/// upstream or route still binds it.
pub async fn delete_plugin(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = chain(&svc, &ctx).await?;
    svc.store().delete_plugin(&id, &chain)?;
    Ok(no_content())
}

/// Resolves the caller's tenant chain once per handler.
///
/// # Errors
///
/// Returns an internal error when the tenant resolver cannot answer.
pub(crate) async fn chain(
    svc: &Arc<ConcreteService>,
    ctx: &SecurityContext,
) -> Result<TenantChain, CanonicalError> {
    svc.chain_for(ctx)
        .await
        .map_err(|err| CanonicalError::internal(format!("tenant resolution failed: {err}")).create())
}

/// Projects the selected fields out of each item.
#[must_use]
fn project(items: Vec<Value>, select: Option<&str>) -> Vec<Value> {
    let Some(select) = select else {
        return items;
    };
    let fields: Vec<String> = select
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    items
        .into_iter()
        .map(|item| {
            let mut projected = serde_json::Map::new();
            if let Value::Object(map) = &item {
                for field in &fields {
                    if let Some(value) = map.get(field) {
                        projected.insert(field.clone(), value.clone());
                    }
                }
            }
            Value::Object(projected)
        })
        .collect()
}

