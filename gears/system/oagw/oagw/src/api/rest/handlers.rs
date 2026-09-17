//! REST handlers.
//!
//! Control-plane handlers are thin adapters over [`ControlPlaneService`]; the
//! data-plane handler converts the inbound HTTP request into a
//! [`ProxyRequest`], which keeps the proxy engine free of axum types.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use uuid::Uuid;

use super::dto::{
    CreatePluginDto, CreateRouteDto, CreateUpstreamDto, PluginPage, ReplaceRouteDto,
    ReplaceUpstreamDto, RoutePage, UpstreamPage, route_input, upstream_input,
};
use super::error::ApiResult;
use crate::domain::error::OagwError;
use crate::domain::services::management::{ControlPlaneService, ListQuery, PluginSource};
use crate::domain::services::proxy::{
    DataPlaneService as _, ProxyRequest, TargetHostChoice, UpstreamResponse,
};
use crate::infra::management::ManagementService;
use crate::infra::proxy::service::DataPlane;

/// Shared handler state.
#[derive(Clone)]
pub struct OagwApi {
    /// Control plane.
    pub management: Arc<ManagementService>,
    /// Data plane.
    pub data_plane: Arc<DataPlane>,
}

/// OData-ish list parameters, `?$top=10&$skip=0&$orderby=created_at desc`.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct ListParams {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    /// Comma-separated projection, e.g. `?$select=id,alias`.
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<usize>,
    #[serde(rename = "$skip")]
    pub skip: Option<usize>,
}

impl ListParams {
    fn to_query(&self) -> ListQuery {
        ListQuery {
            filter: self.filter.clone(),
            select: self.select.as_deref().map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|field| !field.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            }),
            orderby: self.orderby.clone(),
            top: self.top,
            skip: self.skip,
        }
    }
}

// -- upstreams -------------------------------------------------------------

/// `POST /oagw/v1/upstreams`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn create_upstream(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreateUpstreamDto>>,
) -> ApiResult<(StatusCode, axum::Json<crate::domain::model::Upstream>)> {
    let dto = body
        .ok_or_else(|| OagwError::Validation("request body is required".to_owned()))?
        .0;
    let upstream = api.management.create_upstream(&ctx, upstream_input(dto)).await?;
    Ok((StatusCode::CREATED, axum::Json(upstream)))
}

/// `GET /oagw/v1/upstreams`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn list_upstreams(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<axum::Json<UpstreamPage>> {
    let query = params.to_query();
    let items = api.management.list_upstreams(&ctx, &query).await?;
    Ok(axum::Json(UpstreamPage {
        count: items.len(),
        items,
    }))
}

/// `GET /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn get_upstream(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<crate::domain::model::Upstream>> {
    Ok(axum::Json(api.management.get_upstream(&ctx, id).await?))
}

/// `PUT /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn replace_upstream(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    body: Option<axum::Json<ReplaceUpstreamDto>>,
) -> ApiResult<axum::Json<crate::domain::model::Upstream>> {
    let dto = body
        .ok_or_else(|| OagwError::Validation("request body is required".to_owned()))?
        .0;
    Ok(axum::Json(
        api.management
            .replace_upstream(&ctx, id, upstream_input(dto))
            .await?,
    ))
}

/// `DELETE /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn delete_upstream(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    api.management.delete_upstream(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -- routes ----------------------------------------------------------------

/// `POST /oagw/v1/routes`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn create_route(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreateRouteDto>>,
) -> ApiResult<(StatusCode, axum::Json<crate::domain::model::Route>)> {
    let dto = body
        .ok_or_else(|| OagwError::Validation("request body is required".to_owned()))?
        .0;
    let route = api.management.create_route(&ctx, route_input(dto)?).await?;
    Ok((StatusCode::CREATED, axum::Json(route)))
}

/// `GET /oagw/v1/routes`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn list_routes(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<axum::Json<RoutePage>> {
    let query = params.to_query();
    let items = api.management.list_routes(&ctx, &query).await?;
    Ok(axum::Json(RoutePage {
        count: items.len(),
        items,
    }))
}

/// `GET /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn get_route(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<crate::domain::model::Route>> {
    Ok(axum::Json(api.management.get_route(&ctx, id).await?))
}

/// `PUT /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn replace_route(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    body: Option<axum::Json<ReplaceRouteDto>>,
) -> ApiResult<axum::Json<crate::domain::model::Route>> {
    let dto = body
        .ok_or_else(|| OagwError::Validation("request body is required".to_owned()))?
        .0;
    Ok(axum::Json(
        api.management.replace_route(&ctx, id, route_input(dto)?).await?,
    ))
}

/// `DELETE /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn delete_route(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    api.management.delete_route(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -- plugins ---------------------------------------------------------------

/// `POST /oagw/v1/plugins`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn create_plugin(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreatePluginDto>>,
) -> ApiResult<(StatusCode, axum::Json<crate::domain::model::Plugin>)> {
    let dto = body
        .ok_or_else(|| OagwError::Validation("request body is required".to_owned()))?
        .0;
    let plugin = api.management.create_plugin(&ctx, dto).await?;
    Ok((StatusCode::CREATED, axum::Json(plugin)))
}

/// `GET /oagw/v1/plugins`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn list_plugins(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Query(params): Query<ListParams>,
) -> ApiResult<axum::Json<PluginPage>> {
    let query = params.to_query();
    let items = api.management.list_plugins(&ctx, &query).await?;
    Ok(axum::Json(PluginPage {
        count: items.len(),
        items,
    }))
}

/// `GET /oagw/v1/plugins/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn get_plugin(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<axum::Json<crate::domain::model::Plugin>> {
    Ok(axum::Json(api.management.get_plugin(&ctx, &id).await?))
}

/// `DELETE /oagw/v1/plugins/{id}`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn delete_plugin(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    api.management.delete_plugin(&ctx, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source`.
///
/// # Errors
///
/// Propagates [`OagwError`] from the control plane.
pub async fn get_plugin_source(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<axum::Json<PluginSource>> {
    Ok(axum::Json(
        api.management.get_plugin_source(&ctx, &id).await?,
    ))
}

// -- data plane ------------------------------------------------------------

/// Any proxied method against `/oagw/v1/proxy/{alias}[/{path_suffix}]`.
///
/// # Errors
///
/// Gateway errors are returned as problem documents.
pub async fn proxy(
    Extension(api): Extension<Arc<OagwApi>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(rest): Path<String>,
    uri: Uri,
    request: axum::extract::Request,
) -> Response {
    let (alias, suffix) = split_proxy_path(&rest);
    let method = request.method().clone();
    let headers = request.headers().clone();
    let instance = uri.path().to_owned();

    // CORS preflights never touch the upstream and carry no credentials, so
    // they are answered locally (ADR-0004). A bare `OPTIONS` without the
    // preflight headers is an ordinary request and is proxied.
    if method == http::Method::OPTIONS && is_preflight(&headers) {
        return preflight_response(&api, alias, suffix, instance, headers.clone()).await;
    }

    // Read before `headers` is moved into the proxy request.
    let request_target = request_target_host(&uri, &headers);

    let on_upgrade = request
        .extensions()
        .get::<hyper::upgrade::OnUpgrade>()
        .cloned();
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY)
        .await
        .unwrap_or_default();

    let proxy_request = ProxyRequest {
        security_context: ctx,
        alias,
        path_suffix: suffix,
        query: parse_query(uri.query()),
        headers,
        method,
        body,
        target_host: request_target,
        on_upgrade,
        instance,
    };
    execute_proxy(&api, proxy_request).await
}

/// `true` when the request is a browser CORS preflight: `OPTIONS` plus an
/// `Origin` and an `Access-Control-Request-Method`.
fn is_preflight(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Answers a CORS preflight for `alias` without contacting the upstream.
async fn preflight_response(
    api: &OagwApi,
    alias: String,
    suffix: String,
    instance: String,
    headers: HeaderMap,
) -> Response {
    let request = ProxyRequest {
        security_context: toolkit_security::SecurityContext::anonymous(),
        alias,
        path_suffix: suffix,
        query: Vec::new(),
        headers,
        method: http::Method::OPTIONS,
        body: Bytes::new(),
        target_host: TargetHostChoice::Auto,
        on_upgrade: None,
        instance,
    };
    match api.data_plane.preflight(request).await {
        Ok(response) => {
            let status = response.status();
            let mut builder = axum::response::Response::builder().status(status);
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            builder
                .body(axum::body::Body::from(response.into_body()))
                .unwrap_or_else(|_| static_response(status))
        }
        Err(err) => error_response(err),
    }
}

/// Maximum inbound body accepted by the proxy handler.
pub const MAX_BODY: usize = 100 * 1024 * 1024;

/// Renders an upstream response for the caller, splicing WebSocket streams.
fn render_upstream(response: UpstreamResponse) -> Response {
    let status = response.status;
    let mut builder = axum::response::Response::builder().status(status);
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    // ADR-0007: upstream failures pass through verbatim, but the caller must
    // still be able to tell which side produced the failure. `1xx` responses
    // (a completed upgrade) are neither a client nor a server error.
    if status.is_client_error() || status.is_server_error() {
        builder = builder.header(
            crate::domain::error::ErrorSource::HEADER_NAME,
            response.source.as_str(),
        );
    }
    let body = match response.stream {
        Some(stream) => stream.into_axum_body(),
        None => axum::body::Body::from(response.body),
    };
    builder.body(body).unwrap_or_else(|_| static_response(status))
}

/// Renders a gateway error as a problem document.
fn error_response(err: OagwError) -> Response {
    err.into_response()
}

/// Executes a proxy request and renders the upstream response.
async fn execute_proxy(api: &OagwApi, request: ProxyRequest) -> Response {
    match api.data_plane.proxy(request).await {
        Ok(response) => render_upstream(response),
        Err(err) => error_response(err),
    }
}

fn static_response(status: StatusCode) -> Response {
    axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("static response")
}

/// Splits `alias[/suffix]` into its two components.
#[must_use]
pub fn split_proxy_path(rest: &str) -> (String, String) {
    match rest.split_once('/') {
        Some((alias, suffix)) => (alias.to_owned(), format!("/{suffix}")),
        None => (rest.to_owned(), String::new()),
    }
}

/// Resolves the pinned target host for this request.
///
/// The `X-OAGW-Target-Host` header wins; the `target_host` query parameter is
/// the fallback for clients that cannot set arbitrary headers.
#[must_use]
pub fn request_target_host(uri: &Uri, headers: &HeaderMap) -> TargetHostChoice {
    if let Some(value) = headers.get(crate::infra::proxy::headers::TARGET_HOST_HEADER) {
        if let Ok(value) = value.to_str() {
            return TargetHostChoice::Pinned(value.to_owned());
        }
    }
    parse_query(uri.query())
        .into_iter()
        .find(|(name, _)| name == "target_host")
        .map(|(_, value)| TargetHostChoice::Pinned(value))
        .unwrap_or(TargetHostChoice::Auto)
}

/// Parses a query string into `(name, value)` pairs.
#[must_use]
pub fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_path_splits_on_the_first_slash() {
        let (alias, suffix) = split_proxy_path("api.example.com/v1/users/7");
        assert_eq!(alias, "api.example.com");
        assert_eq!(suffix, "/v1/users/7");
        let (alias, suffix) = split_proxy_path("api.example.com");
        assert_eq!(alias, "api.example.com");
        assert_eq!(suffix, "");
    }

    #[test]
    fn target_host_header_wins_over_query() {
        let uri: Uri = "/oagw/v1/proxy/a.local/x?target_host=q.example.com".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(
            request_target_host(&uri, &headers),
            TargetHostChoice::Pinned("q.example.com".to_owned())
        );
        headers.insert(
            crate::infra::proxy::headers::TARGET_HOST_HEADER,
            http::HeaderValue::from_static("h.example.com"),
        );
        assert_eq!(
            request_target_host(&uri, &headers),
            TargetHostChoice::Pinned("h.example.com".to_owned())
        );
    }

    #[test]
    fn no_target_host_hint_is_auto() {
        let uri: Uri = "/oagw/v1/proxy/a.local/x".parse().unwrap();
        assert_eq!(request_target_host(&uri, &HeaderMap::new()), TargetHostChoice::Auto);
    }

    #[test]
    fn query_parsing_keeps_repeated_keys() {
        let query = parse_query(Some("a=1&a=2&b=%20x"));
        assert_eq!(
            query,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("a".to_owned(), "2".to_owned()),
                ("b".to_owned(), " x".to_owned()),
            ]
        );
        assert!(parse_query(None).is_empty());
    }
}
