//! Proxy handlers of the data plane (DESIGN.md §3.2, §3.5).
//!
//! The handlers are deliberately thin: they turn the axum extractors into a
//! [`ProxyRequest`], hand it to [`DataPlaneServiceImpl::proxy`], and render
//! the [`ProxyOutcome`] — an upstream response verbatim, a gateway failure as
//! the gear's problem body. One handler per HTTP method keeps the
//! `OperationBuilder` registrations explicit.

use std::sync::Arc;

use axum::Extension;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::IntoResponse;
use bytes::Bytes;
use toolkit_security::SecurityContext;

use crate::api::rest::body::{ProxyBody, ProxyUpgrade};

use crate::api::rest::error::{OagwError, local_response, upstream_response};
use crate::infra::proxy::{DataPlaneServiceImpl, ProxyOutcome, ProxyRequest};

type DataPlane = Extension<Arc<DataPlaneServiceImpl>>;

/// Proxies `GET /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_get(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "GET", headers, upgrade, body),
    )
    .await
}

/// Proxies `POST /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_post(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "POST", headers, upgrade, body),
    )
    .await
}

/// Proxies `PUT /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_put(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "PUT", headers, upgrade, body),
    )
    .await
}

/// Proxies `PATCH /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_patch(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "PATCH", headers, upgrade, body),
    )
    .await
}

/// Proxies `DELETE /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_delete(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "DELETE", headers, upgrade, body),
    )
    .await
}

/// Proxies `HEAD /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_head(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "HEAD", headers, upgrade, body),
    )
    .await
}

/// Proxies `OPTIONS /oagw/v1/proxy/{alias}/{*path}`.
///
/// # Errors
/// Renders a gateway failure as the gear's problem body.
pub async fn proxy_options(
    data: DataPlane,
    ctx: Extension<SecurityContext>,
    alias: Path<Vec<String>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> Result<impl IntoResponse, OagwError> {
    run(
        data.0,
        ctx.0,
        parts(alias, uri, "OPTIONS", headers, upgrade, body),
    )
    .await
}

/// Extracts `{alias}` and the optional `{*path}` suffix of the request URI.
fn parts(
    alias: Path<Vec<String>>,
    uri: Uri,
    method: &'static str,
    headers: HeaderMap,
    upgrade: ProxyUpgrade,
    body: ProxyBody,
) -> ProxyParts {
    let method = method.to_owned();
    let segments = alias.0;
    // The route is `/proxy/{alias}/{*path}`: axum collects both captures in
    // order, so the suffix is everything after the alias, re-joined.
    let suffix = segments
        .iter()
        .skip(1)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("/");
    let path = if suffix.is_empty() {
        String::new()
    } else {
        format!("/{suffix}")
    };
    ProxyParts {
        alias: segments.first().cloned().unwrap_or_default(),
        path,
        query: uri.query().unwrap_or_default().to_owned(),
        method,
        headers,
        upgrade: upgrade.0,
        body: body.0,
    }
}

/// The pieces a proxy handler extracts from the incoming call.
struct ProxyParts {
    alias: String,
    path: String,
    query: String,
    method: String,
    headers: HeaderMap,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
    body: Bytes,
}

async fn run(
    data: Arc<DataPlaneServiceImpl>,
    ctx: SecurityContext,
    parts: ProxyParts,
) -> Result<axum::response::Response, OagwError> {
    let request = ProxyRequest {
        alias: parts.alias,
        method: parts.method,
        path: parts.path,
        query: parts.query,
        headers: parts.headers,
        upgrade: parts.upgrade,
        body: parts.body,
    };
    match data.proxy(&ctx, request).await? {
        ProxyOutcome::Upstream {
            status,
            headers,
            body,
        } => Ok(upstream_response(status, headers, body)),
        ProxyOutcome::Local {
            status,
            headers,
            body,
        } => Ok(local_response(status, headers, body)),
        ProxyOutcome::Gateway(error) => Err(OagwError::new(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_of(uri: &str, segments: &[&str]) -> ProxyParts {
        parts(
            Path(
                segments
                    .iter()
                    .map(|segment| (*segment).to_owned())
                    .collect(),
            ),
            uri.parse().expect("uri"),
            "GET",
            HeaderMap::new(),
            ProxyUpgrade(None),
            ProxyBody(Bytes::new()),
        )
    }

    #[test]
    fn the_first_capture_is_the_alias_and_the_rest_is_the_suffix() {
        let built = parts_of(
            "http://gateway/oagw/v1/proxy/pets/v1/cats?x=1",
            &["pets", "v1", "cats"],
        );
        assert_eq!(built.alias, "pets");
        assert_eq!(built.path, "/v1/cats");
        assert_eq!(built.query, "x=1");
        assert_eq!(built.method, "GET");
    }

    #[test]
    fn an_empty_suffix_yields_an_empty_path() {
        let built = parts_of("http://gateway/oagw/v1/proxy/pets", &["pets"]);
        assert_eq!(built.alias, "pets");
        assert_eq!(built.path, "");
        assert_eq!(built.query, "");
    }

    #[test]
    fn a_single_segment_suffix_is_joined() {
        let built = parts_of("http://gateway/oagw/v1/proxy/pets/v1", &["pets", "v1"]);
        assert_eq!(built.path, "/v1");
    }
}
