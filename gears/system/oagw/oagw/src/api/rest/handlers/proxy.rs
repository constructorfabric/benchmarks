//! Data-plane proxy handlers.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Extension;
use axum::http::Request;
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::infra::proxy::{ProxyRequest, ws};

use super::super::error::ApiResult;
use super::super::state::OagwState;

/// Prefix every proxy route is mounted under.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// `{any} /oagw/v1/proxy/{alias}[/{*path_suffix}]`
///
/// The alias is the first segment after the mount point and everything after
/// it is forwarded verbatim, so percent-encoding and query order survive.
///
/// # Errors
///
/// Returns the problem document of any resolution, validation, plugin,
/// rate-limit, CORS or transport failure of the data plane.
pub async fn proxy(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    mut request: Request<Body>,
) -> ApiResult<Response> {
    let upgrade = ws::take_upgrade(&mut request);
    let (parts, body) = request.into_parts();
    let (alias, suffix) = split_target(parts.uri.path()).ok_or_else(unknown_alias)?;
    let query = parse_query(parts.uri.query().unwrap_or_default());
    let proxied = ProxyRequest {
        context: ctx,
        method: parts.method.clone(),
        alias,
        path: path_to_forward(&suffix),
        query,
        headers: parts.headers.clone(),
        body,
        upgrade,
    };
    Ok(state.proxy.execute(proxied).await?)
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod proxy_tests;

/// Split a proxied path into the upstream alias and the forwarded suffix.
///
/// `/oagw/v1/proxy/alias` forwards an empty suffix and
/// `/oagw/v1/proxy/alias/a/b` forwards `/a/b`, keeping the leading slash so
/// the suffix can be appended to a configured route path unchanged.
#[must_use]
pub fn split_target(path: &str) -> Option<(String, String)> {
    let rest = path
        .strip_prefix(PROXY_PREFIX)
        .filter(|rest| !rest.is_empty())?;
    Some(match rest.find('/') {
        Some(index) => (rest[..index].to_owned(), rest[index..].to_owned()),
        None => (rest.to_owned(), String::new()),
    })
}

/// The path handed to [`crate::infra::proxy::http::upstream_uri`].
fn path_to_forward(suffix: &str) -> String {
    if suffix.is_empty() {
        "/".to_owned()
    } else {
        suffix.to_owned()
    }
}

/// The routing error for a request without an alias under the mount point.
fn unknown_alias() -> crate::domain::error::DomainError {
    crate::domain::error::DomainError::route_not_found(format!(
        "no upstream alias under `{PROXY_PREFIX}`"
    ))
}

/// Decode a query string into wire-order pairs.
///
/// `form_urlencoded` decodes `+` as a space, which is how a form-encoded query
/// is defined to behave; percent escapes are decoded either way.
#[must_use]
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
