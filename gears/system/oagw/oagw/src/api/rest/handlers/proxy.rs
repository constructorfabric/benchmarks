//! The proxy handler.
//!
//! Two routes reach the same body: `/oagw/v1/proxy/{alias}` and
//! `/oagw/v1/proxy/{alias}/{*path}`. Both accept any method, because the
//! method is part of what is proxied.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, Path, Request};
use axum::response::{IntoResponse, Response};
use http::{HeaderName, HeaderValue, StatusCode, header};
use toolkit_security::SecurityContext;

use crate::api::rest::error::problem_response;
use crate::api::rest::state::{OagwState, actions};
use crate::domain::cors;
use crate::domain::error::ErrorSource;
use crate::domain::gts_helpers;
use crate::domain::services::proxy::{ERROR_SOURCE_HEADER, ProxyRequest};

/// `{METHOD} /oagw/v1/proxy/{alias}`
pub async fn proxy_root(
    Extension(state): Extension<Arc<OagwState>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    handle(state, alias, None, request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path}`
pub async fn proxy_with_path(
    Extension(state): Extension<Arc<OagwState>>,
    Path((alias, suffix)): Path<(String, String)>,
    request: Request,
) -> Response {
    handle(state, alias, Some(suffix), request).await
}

async fn handle(
    state: Arc<OagwState>,
    alias: String,
    suffix: Option<String>,
    mut request: Request,
) -> Response {
    let instance = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| request.uri().path().to_owned(), |u| u.0.path().to_owned());

    // A browser preflight carries no credentials, so there is no tenant to
    // resolve an upstream with: answer it here, permissively, and defer
    // origin enforcement to the actual request (`ADR/0004-cors.md`).
    let origin = header_str(&request, header::ORIGIN.as_str());
    let request_method = header_str(&request, "access-control-request-method");
    if cors::is_preflight(
        request.method().as_str(),
        origin.as_deref(),
        request_method.as_deref(),
    ) {
        let request_headers = header_str(&request, "access-control-request-headers");
        return preflight_response(&origin.unwrap_or_default(), request_method, request_headers);
    }

    let Some(security_context) = request.extensions().get::<SecurityContext>().cloned() else {
        // Only reachable if the gateway's auth layer is not in the stack.
        let err = crate::domain::DomainError::authentication_failed(
            "no security context is attached to this request",
        );
        return problem_response(&err, &instance, ErrorSource::Gateway);
    };

    if let Err(err) = state
        .authorize(&security_context, gts_helpers::PROXY_TYPE, actions::INVOKE)
        .await
    {
        return problem_response(&err, &instance, ErrorSource::Gateway);
    }

    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let on_upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();

    let (parts, body) = request.into_parts();
    let proxy_request = ProxyRequest {
        security_context,
        alias,
        path_suffix: suffix,
        method: parts.method.clone(),
        query: parts.uri.query().map(str::to_owned),
        headers: parts.headers,
        body,
        on_upgrade,
        client_ip,
        instance: instance.clone(),
    };

    match state.data_plane.execute(proxy_request).await {
        Ok(response) => response,
        Err(err) => problem_response(&err, &instance, ErrorSource::Gateway),
    }
}

fn header_str(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Permissive `204` echoing the preflight request, per `ADR/0004-cors.md`
/// §"Preflight Request Handling".
fn preflight_response(
    origin: &str,
    request_method: Option<String>,
    request_headers: Option<String>,
) -> Response {
    let echo = cors::preflight_echo(origin, request_method.as_deref(), request_headers.as_deref());
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();

    let mut set = |name: &'static str, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    };
    set("access-control-allow-origin", &echo.origin);
    if let Some(methods) = &echo.methods {
        set("access-control-allow-methods", methods);
    }
    if let Some(request_headers) = &echo.headers {
        set("access-control-allow-headers", request_headers);
    }
    set(
        "access-control-max-age",
        &cors::PREFLIGHT_MAX_AGE_SECS.to_string(),
    );
    set("vary", cors::PREFLIGHT_VARY);
    set(ERROR_SOURCE_HEADER, ErrorSource::Gateway.as_str());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_echoes_origin_method_and_headers() {
        let response = preflight_response(
            "https://app.example.com",
            Some("POST".to_owned()),
            Some("Content-Type, Authorization".to_owned()),
        );
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(headers["access-control-allow-methods"], "POST");
        assert_eq!(
            headers["access-control-allow-headers"],
            "Content-Type, Authorization"
        );
        assert_eq!(headers["access-control-max-age"], "86400");
        assert_eq!(headers["vary"], cors::PREFLIGHT_VARY);
    }
}
