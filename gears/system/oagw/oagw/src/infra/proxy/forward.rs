//! The proxy hop: build the upstream request, send it, stream the answer back.
//!
//! Bodies are never buffered. The inbound request body is handed to hyper as an
//! axum `Body` (which streams), and the upstream response body is re-attached
//! as a stream, so `text/event-stream` and other chunked payloads reach the
//! caller chunk by chunk. A WebSocket upgrade is bridged socket to socket once
//! the upstream answers `101`.
//!
//! The hyper client feature set (`client`, `http1`) is enabled for `hyper` by
//! this crate's `toolkit-http` dependency, so no feature is added here.
use std::time::Duration;

use http::header::UPGRADE;
use http::{HeaderMap, Method, Version};
use hyper_util::rt::TokioIo;

use crate::domain::model::Endpoint;
use crate::infra::proxy::connector::Connection;
use crate::infra::proxy::failure::ProxyFailure;
use crate::infra::proxy::headers::{HOST_HEADER, strip_response_hop_by_hop};

/// Upstream response body: unbuffered, so SSE frames arrive one at a time.
pub type UpstreamBody = hyper::body::Incoming;

/// Whether the inbound request asked for a protocol upgrade.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap, method: &Method) -> bool {
    if *method == Method::CONNECT {
        return true;
    }
    let connection = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let wants_upgrade = headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    wants_upgrade
        && connection
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// The absolute-form URI of an upstream request.
///
/// # Errors
///
/// [`ProxyFailure::validation`] when the pieces do not form a URI.
#[allow(clippy::result_large_err)]
pub fn upstream_uri(
    endpoint: &Endpoint,
    path: &str,
    query: Option<&str>,
) -> Result<http::Uri, ProxyFailure> {
    let authority = endpoint.host.contains(':').then(|| endpoint.host.clone());
    let host = match authority {
        Some(_) => format!("[{}]", endpoint.host.trim_matches(|c| c == '[' || c == ']')),
        None => endpoint.host.clone(),
    };
    let authority = format!("{host}:{}", endpoint.port);
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let path_and_query = match query.filter(|query| !query.is_empty()) {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    http::Uri::builder()
        .scheme(endpoint_scheme(endpoint))
        .authority(authority)
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| {
            ProxyFailure::validation(format!("cannot build the upstream request URI: {error}"))
        })
}

/// The URI scheme string of a pooled endpoint.
fn endpoint_scheme(endpoint: &Endpoint) -> &'static str {
    if endpoint.scheme.is_plaintext() {
        "http"
    } else {
        "https"
    }
}

/// Build the upstream request.
///
/// `forwarded` holds the headers that survived the inbound passthrough and the
/// plugin phases; `Host` is rewritten to the selected endpoint, which is what a
/// shared endpoint pool needs.
///
/// # Errors
///
/// [`ProxyFailure::validation`] when a header value is not renderable.
#[allow(clippy::result_large_err)]
pub fn build_request(
    method: &Method,
    uri: http::Uri,
    forwarded: HeaderMap,
    endpoint: &Endpoint,
    body: axum::body::Body,
) -> Result<http::Request<axum::body::Body>, ProxyFailure> {
    let host = if endpoint.port == endpoint.scheme.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    let host = http::HeaderValue::try_from(host)
        .map_err(|error| ProxyFailure::validation(format!("invalid upstream host: {error}")))?;

    let mut builder = http::Request::builder()
        .method(method.clone())
        .uri(uri)
        .version(Version::HTTP_11)
        .header(HOST_HEADER, host);
    for (name, value) in forwarded.iter() {
        builder = builder.header(name.clone(), value.clone());
    }
    builder.body(body).map_err(|error| {
        ProxyFailure::validation(format!("cannot build the upstream request: {error}"))
    })
}

/// Rewrite the request target into origin-form.
///
/// The data plane dials the socket itself, so the hop is an ordinary origin
/// request: hyper's h1 client writes `RequestLine(method, uri)` verbatim
/// (`proto::RequestLine(parts.method, parts.uri)` in `proto/h1/dispatch.rs`),
/// and an absolute-form target makes every upstream that does not act as a
/// proxy answer 404. The authority travels in the `Host` header instead.
pub fn set_origin_form<B>(request: &mut http::Request<B>) {
    let uri = request.uri();
    let is_absolute = uri.scheme().is_some() || uri.authority().is_some();
    if !is_absolute {
        return;
    }
    let path_and_query = match uri.path_and_query() {
        Some(path) if path.as_str() != "/" => path.clone(),
        _ => http::Uri::default()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| "/".parse().expect("'/' is a valid path")),
    };
    if let Ok(origin) = http::Uri::builder()
        .path_and_query(path_and_query.as_str())
        .build()
    {
        *request.uri_mut() = origin;
    }
}

/// Perform the upstream HTTP/1.1 exchange.
///
/// The dial already happened; `connection` is the live socket. The configured
/// timeout bounds the time until the response *head* arrives — it is
/// deliberately not applied to the body, because a long-lived
/// `text/event-stream` is not a stalled exchange.
///
/// Returns the response and whether the caller asked for an upgrade.
///
/// # Errors
///
/// [`ProxyFailure`] when the handshake, the write or the response head fails.
pub async fn exchange(
    connection: Connection,
    request: http::Request<axum::body::Body>,
    timeout: Duration,
) -> Result<(http::Response<UpstreamBody>, bool), ProxyFailure> {
    match connection {
        crate::infra::proxy::connector::Connection::Plain(io) => {
            send(io, request, timeout, false).await
        }
        crate::infra::proxy::connector::Connection::Tls(io) => {
            send(io, request, timeout, true).await
        }
    }
}

async fn send<T>(
    io: TokioIo<T>,
    request: http::Request<axum::body::Body>,
    timeout: Duration,
    _tls: bool,
) -> Result<(http::Response<UpstreamBody>, bool), ProxyFailure>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection, request) = {
        let mut request = request;
        set_origin_form(&mut request);
        // `with_upgrades` is what lets `hyper::upgrade::on(&mut response)`
        // hand the socket over once the upstream answers `101`; without it the
        // upgrade future fails with "upgrade expected but low level API in
        // use".
        let (sender, connection) = hyper::client::conn::http1::Builder::new()
            .handshake(io)
            .await
            .map_err(|error| {
                ProxyFailure::protocol_error(format!("upstream handshake failed: {error}"))
            })?;
        (sender, connection.with_upgrades(), request)
    };
    let upgrade = request
        .headers()
        .get(UPGRADE)
        .is_some_and(|value| !value.is_empty());

    // Drive the connection in the background: the exchange awaits the response
    // here, and an upgrade hands the socket over from that same task.
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(error = %error, "oagw: upstream connection closed");
        }
    });

    let response = match tokio::time::timeout(timeout, sender.send_request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(exchange_failure(&error)),
        Err(_) => {
            return Err(ProxyFailure::request_timeout(
                "upstream exchange exceeded the configured timeout",
            ));
        }
    };
    Ok((response, upgrade))
}

/// Map a hyper exchange error onto the catalogue.
fn exchange_failure(error: &hyper::Error) -> ProxyFailure {
    if error.is_timeout() {
        ProxyFailure::request_timeout(error.to_string())
    } else if error.is_incomplete_message() {
        ProxyFailure::new(
            502,
            crate::domain::plugin::STREAM_ABORTED,
            "Stream Aborted",
            format!("upstream closed the response early: {error}"),
        )
    } else if error.is_parse() || error.is_parse_status() {
        ProxyFailure::protocol_error(format!("upstream spoke an unusable protocol: {error}"))
    } else {
        ProxyFailure::link_unavailable(format!("upstream link failed: {error}"))
    }
}

/// Drop hop-by-hop headers from an upstream response before forwarding it.
#[must_use]
pub fn prepare_response_headers(headers: HeaderMap, upgrade: bool) -> HeaderMap {
    let mut headers = headers;
    strip_response_hop_by_hop(&mut headers, upgrade);
    headers
}

/// The outbound header set: the inbound passthrough result with the two
/// headers the gateway owns (`Host`, `X-OAGW-Target-Host`) and the framing
/// headers hyper owns (`Content-Length`) removed.
///
/// hyper derives framing from the body it is handed, so a forwarded
/// `Content-Length` would disagree with the body it re-encodes.
#[must_use]
pub fn prepare_request_headers(headers: HeaderMap) -> HeaderMap {
    let mut headers = headers;
    for name in [
        crate::infra::proxy::headers::HOST_HEADER,
        crate::infra::proxy::headers::TARGET_HOST_HEADER,
        "content-length",
    ] {
        if let Ok(name) = http::HeaderName::try_from(name) {
            headers.remove(name);
        }
    }
    headers
}

#[cfg(test)]
#[path = "forward_tests.rs"]
mod tests;
