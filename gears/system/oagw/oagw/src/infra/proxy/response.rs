//! The proxy response, as the data plane hands it to the transport layer.
//!
//! A body is either the untouched upstream stream — the normal case, and the
//! only case that preserves SSE chunking — or an already-upgraded socket, which
//! the transport layer bridges with the inbound one.
use http::HeaderMap;
use std::time::Duration;

use crate::domain::model::CorsConfig;
use crate::infra::proxy::cors::CorsRequest;
use crate::infra::proxy::failure::ErrorSource;

/// A proxied response, ready for the wire.
#[derive(Debug)]
pub struct ProxyResponse {
    /// Upstream status code.
    pub status: u16,
    /// Headers to send, after hop-by-hop stripping and header rewriting.
    pub headers: HeaderMap,
    /// Upstream body, streamed.
    pub body: axum::body::Body,
    /// The upstream's upgraded socket, when the caller asked for an upgrade.
    pub upgraded: Option<hyper::upgrade::Upgraded>,
    /// Which plane owns the response.
    pub source: ErrorSource,
}

impl ProxyResponse {
    /// A 204 preflight answer with the given CORS headers.
    #[must_use]
    pub fn preflight(headers: Vec<(&'static str, String)>) -> Self {
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name),
                http::HeaderValue::try_from(value),
            ) {
                header_map.insert(name, value);
            }
        }
        Self {
            status: http::StatusCode::NO_CONTENT.as_u16(),
            headers: header_map,
            body: axum::body::Body::empty(),
            upgraded: None,
            source: ErrorSource::Gateway,
        }
    }
}

/// Attach the CORS headers a forwarded cross-origin response carries.
#[must_use]
pub fn cors_headers(
    config: Option<&CorsConfig>,
    request: &CorsRequest,
) -> Option<Vec<(&'static str, String)>> {
    let origin = request
        .origin
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())?;
    let config = config.filter(|config| config.enabled)?;
    if !crate::infra::proxy::cors::origin_allowed(config, origin) {
        return None;
    }
    let mut headers = vec![
        (crate::infra::proxy::cors::ALLOW_ORIGIN, origin.to_owned()),
        (crate::infra::proxy::cors::VARY, "Origin".to_owned()),
    ];
    if config.allow_credentials {
        headers.push((
            crate::infra::proxy::cors::ALLOW_CREDENTIALS,
            "true".to_owned(),
        ));
    }
    if !config.expose_headers.is_empty() {
        headers.push((
            crate::infra::proxy::cors::EXPOSE_HEADERS,
            config.expose_headers.join(", "),
        ));
    }
    Some(headers)
}

/// Whether a response of this media type is streamed rather than buffered.
#[must_use]
pub fn is_streaming_media_type(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    media_type.starts_with("text/event-stream")
        || media_type.starts_with("application/grpc")
        || media_type.starts_with("application/stream+json")
        || media_type.starts_with("application/x-ndjson")
        || media_type.starts_with("multipart/x-mixed-replace")
        || media_type.starts_with("application/octet-stream")
}

/// The `proxy_timeout_secs` default, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 2;

/// A timeout of `secs` seconds.
#[must_use]
pub const fn exchange_timeout(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// Upper bound on a *buffered* upstream response, in bytes.
///
/// Only responses the plugin phases must be able to inspect are buffered; a
/// larger payload is forwarded unbuffered, which is never wrong for a proxy.
pub const MAX_BUFFERED_BODY: usize = 8 * 1024 * 1024;

/// Read a body into memory, bounded by [`MAX_BUFFERED_BODY`].
///
/// # Errors
///
/// Never: a body that cannot be read is treated as empty, so the hop still
/// produces a response.
pub async fn buffer_body(body: axum::body::Body) -> Option<bytes::Bytes> {
    axum::body::to_bytes(body, MAX_BUFFERED_BODY)
        .await
        .ok()
        .filter(|bytes| !bytes.is_empty())
}
