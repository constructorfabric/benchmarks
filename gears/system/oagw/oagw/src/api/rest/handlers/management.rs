//! Management handlers: upstream, route and plugin CRUD.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use serde_json::Value;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::convert;
use crate::api::rest::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, ListParams, PluginDto,
    PluginSourceDto, RouteDto, UpdateRouteRequest, UpdateUpstreamRequest, UpstreamDto,
};
use crate::api::rest::dto::MergeUpdate;
use crate::api::rest::error::OagwError;
use crate::api::rest::list::ListQuery;
use crate::domain::services::management::ControlPlane;

fn query(params: &ListParams) -> ListQuery {
    ListQuery {
        orderby: params
            .orderby
            .as_deref()
            .and_then(crate::api::rest::list::parse_orderby),
        filter: params
            .filter
            .as_deref()
            .and_then(crate::api::rest::list::parse_filter),
        select: params
            .select
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|field| !field.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        skip: params.skip.unwrap_or(0),
        top: params
            .top
            .unwrap_or(crate::api::rest::list::DEFAULT_TOP)
            .min(crate::api::rest::list::MAX_TOP),
    }
}

fn to_page<T, F>(items: &[T], render: F, params: &ListParams) -> Vec<Value>
where
    F: Fn(&T) -> Value,
{
    let rendered: Vec<Value> = items.iter().map(render).collect();
    let parsed = query(params);
    parsed.project(parsed.apply(rendered))
}

/// Renders the page envelope the management API returns.
fn page_json(items: &[Value]) -> Value {
    serde_json::json!({
        "context": { "page": { "count": items.len(), "limit": 100, "start": 0 } },
        "data": items,
    })
}

/// The current instant as an RFC 3339 timestamp, with second precision.
fn stamp() -> String {
    stamp_from_unix(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    )
}

/// Formats a Unix timestamp as RFC 3339, with second precision.
///
// The divisions below are the calendar arithmetic itself: they are exact by
// construction, so switching to floats would only add rounding error.
#[allow(clippy::integer_division)]
fn stamp_from_unix(seconds: u64) -> String {
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// Converts days since the Unix epoch to a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`: the shift into a March-based year makes
/// the leap rule a plain division, so no per-month table is needed.
///
// Every division here is a deliberate truncating division; the algorithm's
// correctness depends on it, so the exact integer arithmetic is kept.
#[allow(clippy::integer_division)]
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    let shifted = i64::try_from(days).unwrap_or(i64::MAX) + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// `POST /oagw/v1/upstreams`
///
/// # Errors
/// Returns an [`OagwError`] when validation fails or the alias is taken.
pub async fn create_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Json(request): Json<CreateUpstreamRequest>,
) -> Result<(StatusCode, Json<UpstreamDto>), OagwError> {
    let mut upstream = convert::to_upstream(&request, Uuid::new_v4(), context.subject_tenant_id())?;
    upstream.created_at = Some(stamp());
    let created = cp
        .create_upstream(context.subject_tenant_id(), upstream)
        .await
        .map_err(OagwError::from)?;
    Ok((StatusCode::CREATED, Json(convert::from_upstream(&created))))
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
/// Returns an [`OagwError`] when the store fails.
pub async fn list_upstreams(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, OagwError> {
    let items = cp
        .list_upstreams(context.subject_tenant_id())
        .await
        .map_err(OagwError::from)?;
    Ok(Json(page_json(&to_page(
        &items,
        |upstream| serde_json::to_value(convert::from_upstream(upstream)).unwrap_or_default(),
        &params,
    ))))
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn get_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<UpstreamDto>, OagwError> {
    let upstream = cp
        .get_upstream(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(Json(convert::from_upstream(&upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when validation fails or the resource is absent.
pub async fn replace_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateUpstreamRequest>,
) -> Result<Json<UpstreamDto>, OagwError> {
    let tenant = context.subject_tenant_id();
    let existing = cp.get_upstream(tenant, id).await.map_err(OagwError::from)?;
    let stored = convert::upstream_to_update_request(&existing);
    let merged = request.merged_with(&stored);
    let mut next = convert::to_upstream_update(&merged, &existing)?;
    next.updated_at = Some(stamp());
    let updated = cp
        .replace_upstream(tenant, id, next)
        .await
        .map_err(OagwError::from)?;
    Ok(Json(convert::from_upstream(&updated)))
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn delete_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    cp.delete_upstream(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/routes`
///
/// # Errors
/// Returns an [`OagwError`] when validation fails.
pub async fn create_route(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Json(request): Json<CreateRouteRequest>,
) -> Result<(StatusCode, Json<RouteDto>), OagwError> {
    let mut route = convert::to_route(&request, Uuid::new_v4(), context.subject_tenant_id())?;
    route.created_at = Some(stamp());
    let created = cp
        .create_route(context.subject_tenant_id(), route)
        .await
        .map_err(OagwError::from)?;
    Ok((StatusCode::CREATED, Json(convert::from_route(&created))))
}

/// `GET /oagw/v1/routes`
///
/// # Errors
/// Returns an [`OagwError`] when the store fails.
pub async fn list_routes(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, OagwError> {
    let items = cp
        .list_routes(context.subject_tenant_id())
        .await
        .map_err(OagwError::from)?;
    Ok(Json(page_json(&to_page(
        &items,
        |route| serde_json::to_value(convert::from_route(route)).unwrap_or_default(),
        &params,
    ))))
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn get_route(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<RouteDto>, OagwError> {
    let route = cp
        .get_route(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(Json(convert::from_route(&route)))
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when validation fails.
pub async fn replace_route(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateRouteRequest>,
) -> Result<Json<RouteDto>, OagwError> {
    let tenant = context.subject_tenant_id();
    let existing = cp.get_route(tenant, id).await.map_err(OagwError::from)?;
    let stored = convert::route_to_update_request(&existing);
    let merged = request.merged_with(&stored);
    let mut next = convert::to_route_update(&merged, &existing)?;
    next.updated_at = Some(stamp());
    let updated = cp.replace_route(tenant, id, next).await.map_err(OagwError::from)?;
    Ok(Json(convert::from_route(&updated)))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn delete_route(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    cp.delete_route(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/plugins`
///
/// # Errors
/// Returns an [`OagwError`] when validation fails.
pub async fn create_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Json(request): Json<CreatePluginRequest>,
) -> Result<(StatusCode, Json<PluginDto>), OagwError> {
    let plugin = convert::to_plugin(&request, Uuid::new_v4(), context.subject_tenant_id())?;
    let created = cp
        .create_plugin(context.subject_tenant_id(), plugin)
        .await
        .map_err(OagwError::from)?;
    Ok((StatusCode::CREATED, Json(convert::from_plugin(&created))))
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
/// Returns an [`OagwError`] when the store fails.
pub async fn list_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, OagwError> {
    let items = cp
        .list_plugins(context.subject_tenant_id())
        .await
        .map_err(OagwError::from)?;
    Ok(Json(page_json(&to_page(
        &items,
        |plugin| serde_json::to_value(convert::from_plugin(plugin)).unwrap_or_default(),
        &params,
    ))))
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn get_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<PluginDto>, OagwError> {
    let plugin = cp
        .get_plugin(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(Json(convert::from_plugin(&plugin)))
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
/// Returns an [`OagwError`] when the plugin is referenced.
pub async fn delete_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    cp.delete_plugin(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
/// Returns an [`OagwError`] when the resource is absent.
pub async fn get_plugin_source(
    Extension(context): Extension<SecurityContext>,
    Extension(cp): Extension<Arc<dyn ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<PluginSourceDto>, OagwError> {
    let plugin = cp
        .get_plugin(context.subject_tenant_id(), id)
        .await
        .map_err(OagwError::from)?;
    Ok(Json(PluginSourceDto {
        id: plugin.id,
        name: plugin.name,
        source: plugin.source,
    }))
}

#[cfg(test)]
mod civil_tests {
    use super::civil_from_days;

    #[test]
    fn the_epoch_is_new_years_day_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_dates_round_trip() {
        assert_eq!(civil_from_days(20_698), (2026, 9, 2));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_697), (2026, 9, 1));
    }

    #[test]
    fn stamps_are_rfc3339() {
        assert_eq!(super::stamp_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            super::stamp_from_unix(1_788_177_600),
            "2026-08-31T12:00:00Z"
        );
    }
}
