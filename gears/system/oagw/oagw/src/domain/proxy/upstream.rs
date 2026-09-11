//! Upstream invocation: the plaintext-connection policy, target-URL
//! construction, and the timed `reqwest` call with transport-failure
//! classification (`cpt-cf-oagw-algo-upstream-invocation`,
//! `cpt-cf-oagw-dod-plaintext-connection-policy`, `cpt-cf-oagw-dod-proxy-timeout`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, StatusCode};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use crate::domain::model::Scheme;
use crate::error::OagwError;

/// The plaintext-connection policy (controller decision D3), evaluated
/// immediately before a connection is opened to an `http` or `ws` endpoint
/// (`cpt-cf-oagw-dod-plaintext-connection-policy`). This exact function is
/// reused, unchanged, by the streaming feature's `ws` upgrade path.
///
/// # Errors
///
/// Returns [`OagwError::link_unavailable`] (`503`) when `scheme` is `http`
/// or `ws` and `allow_http_upstream` is `false`.
// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-policy-fn-01
pub fn enforce_plaintext_policy(
    scheme: Scheme,
    allow_http_upstream: bool,
) -> Result<(), OagwError> {
    let is_plaintext = matches!(scheme, Scheme::Http | Scheme::Ws);
    if is_plaintext && !allow_http_upstream {
        Err(OagwError::link_unavailable(format!(
            "plaintext upstream connections are disabled (scheme '{}')",
            scheme_str(scheme)
        )))
    } else {
        Ok(())
    }
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-policy-fn-01

/// The wire-form scheme spelling, used both for the plaintext-policy
/// message and target-URL construction.
const fn scheme_str(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Http => "http",
        Scheme::Https => "https",
        Scheme::Ws => "ws",
        Scheme::Wss => "wss",
        Scheme::Wt => "wt",
        Scheme::Grpc => "grpc",
    }
}

/// Builds the outbound target URL from the selected endpoint and the
/// resolved request path/query (`cpt-cf-oagw-algo-upstream-invocation`).
///
/// # Errors
///
/// Returns [`OagwError::protocol_error`] (`502`) when `scheme` cannot serve
/// a plain HTTP proxy request (any scheme other than `http`/`https`) —
/// reserved for `ws`/`wss`/`wt`/`grpc` endpoints, which this feature does
/// not proxy.
pub fn build_target_url(
    scheme: Scheme,
    host: &str,
    port: u16,
    path_and_query: &str,
) -> Result<url::Url, OagwError> {
    if !matches!(scheme, Scheme::Http | Scheme::Https) {
        return Err(OagwError::protocol_error(format!(
            "endpoint scheme '{}' cannot serve a plain HTTP proxy request",
            scheme_str(scheme)
        )));
    }
    let raw = format!("{}://{host}:{port}{path_and_query}", scheme_str(scheme));
    url::Url::parse(&raw)
        .map_err(|_| OagwError::validation_error(format!("'{raw}' is not a valid target URL")))
}

/// A complete upstream response: status, headers, and body, forwarded
/// unmodified (`cpt-cf-oagw-algo-error-mapping`).
pub struct UpstreamOutcome {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// An upstream response whose headers have arrived but whose body has not
/// yet been read, used by the streaming feature
/// (`cpt-cf-oagw-algo-stream-detection`) to decide, from `response`'s status
/// and headers alone, whether to switch to pass-through streaming
/// (`cpt-cf-oagw-algo-incremental-forwarding`) or fall back to the buffered
/// path via [`finish_buffered`]. `deadline` is private: only
/// [`finish_buffered`] bounds a further read against it, since a stream that
/// switches to pass-through must never be bounded by it again
/// (`cpt-cf-oagw-dod-longlived-pipeline`).
pub struct UpstreamHead {
    pub response: reqwest::Response,
    deadline: Instant,
}

/// Sends one request to the selected upstream endpoint and awaits its
/// response status and headers only, bounding connection establishment and
/// the response head by `timeout` — never the body that follows
/// (`cpt-cf-oagw-algo-upstream-invocation`, `cpt-cf-oagw-dod-proxy-timeout`,
/// `cpt-cf-oagw-dod-longlived-pipeline`).
///
/// # Errors
///
/// Returns [`OagwError::connection_timeout`] when the connection cannot be
/// established in time, [`OagwError::request_timeout`] when the response
/// headers never arrive in time, and [`OagwError::downstream_error`] for any
/// other transport failure (a refused connection, DNS failure, or
/// mid-negotiation transport error).
// @cpt-begin:cpt-cf-oagw-dod-proxy-timeout:p1:inst-upstream-invoke-head-fn-01
// @cpt-begin:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-upstream-invoke-head-fn-01
pub async fn invoke_upstream_head(
    client: &reqwest::Client,
    method: Method,
    url: url::Url,
    headers: HeaderMap,
    body: Bytes,
    timeout: Duration,
) -> Result<UpstreamHead, OagwError> {
    let deadline = Instant::now() + timeout;
    let mut builder = client.request(method, url).headers(headers);
    if !body.is_empty() {
        builder = builder.body(body);
    }

    let response = await_response(builder, deadline).await?;
    Ok(UpstreamHead { response, deadline })
}
// @cpt-end:cpt-cf-oagw-dod-longlived-pipeline:p1:inst-upstream-invoke-head-fn-01
// @cpt-end:cpt-cf-oagw-dod-proxy-timeout:p1:inst-upstream-invoke-head-fn-01

/// Reads `head`'s body to completion, still bounded by its original
/// deadline: the buffered (non-streaming) path's idle-timeout behavior,
/// unchanged from before this feature split header retrieval out of
/// [`invoke_upstream`] (`cpt-cf-oagw-dod-proxy-timeout`).
///
/// # Errors
///
/// Returns [`OagwError::idle_timeout`] when the response body stream stalls
/// past the remaining deadline, and [`OagwError::downstream_error`] for any
/// other mid-stream transport failure.
pub async fn finish_buffered(head: UpstreamHead) -> Result<UpstreamOutcome, OagwError> {
    let status = head.response.status();
    let response_headers = head.response.headers().clone();
    let body = collect_body(head.response, head.deadline).await?;

    Ok(UpstreamOutcome {
        status,
        headers: response_headers,
        body,
    })
}

/// Forwards one request to the selected upstream endpoint and reads its
/// complete response, bounding connection establishment, response
/// completion, and body-stream stalls by `timeout`
/// (`cpt-cf-oagw-algo-upstream-invocation`, `cpt-cf-oagw-dod-proxy-timeout`).
///
/// # Errors
///
/// Returns [`OagwError::connection_timeout`] when the connection cannot be
/// established in time, [`OagwError::request_timeout`] when the response
/// headers never arrive in time, [`OagwError::idle_timeout`] when the
/// response body stream stalls past the remaining deadline, and
/// [`OagwError::downstream_error`] for any other transport failure (a
/// refused connection, DNS failure, or mid-stream transport error).
// @cpt-begin:cpt-cf-oagw-dod-proxy-timeout:p1:inst-upstream-invoke-fn-01
pub async fn invoke_upstream(
    client: &reqwest::Client,
    method: Method,
    url: url::Url,
    headers: HeaderMap,
    body: Bytes,
    timeout: Duration,
) -> Result<UpstreamOutcome, OagwError> {
    let head = invoke_upstream_head(client, method, url, headers, body, timeout).await?;
    finish_buffered(head).await
}

/// Awaits the response headers within the remaining deadline
/// (`cpt-cf-oagw-dod-proxy-timeout`): elapsing here means "a response that
/// never completes" and maps to [`OagwError::request_timeout`], distinct
/// from a connect-phase timeout (which `reqwest`'s own `connect_timeout`
/// classifies as [`OagwError::connection_timeout`] before this future even
/// resolves).
async fn await_response(
    builder: reqwest::RequestBuilder,
    deadline: Instant,
) -> Result<reqwest::Response, OagwError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(remaining, builder.send()).await {
        Err(_) => Err(OagwError::request_timeout(
            "upstream did not respond within the configured proxy timeout",
        )),
        Ok(Err(error)) => Err(classify_transport_error(&error)),
        Ok(Ok(response)) => Ok(response),
    }
}

/// Reads the response body to completion, one chunk at a time, each await
/// bounded by the remaining deadline: a chunk wait that elapses after
/// headers were already received is a stalled data flow, mapped to
/// [`OagwError::idle_timeout`] (`cpt-cf-oagw-dod-proxy-timeout`).
async fn collect_body(response: reqwest::Response, deadline: Instant) -> Result<Bytes, OagwError> {
    let mut stream = response.bytes_stream();
    let mut collected = BytesMut::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_) => {
                return Err(OagwError::idle_timeout(
                    "upstream response body stalled past the configured proxy timeout",
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(classify_transport_error(&error)),
            Ok(Some(Ok(chunk))) => collected.extend_from_slice(&chunk),
        }
    }
    Ok(collected.freeze())
}

/// Classifies a `reqwest` transport failure into the documented gateway
/// error types (`cpt-cf-oagw-algo-upstream-invocation`).
fn classify_transport_error(error: &reqwest::Error) -> OagwError {
    if error.is_timeout() && error.is_connect() {
        OagwError::connection_timeout(format!("connection to upstream timed out: {error}"))
    } else if error.is_timeout() {
        OagwError::request_timeout(format!("upstream request timed out: {error}"))
    } else {
        OagwError::downstream_error(format!("upstream transport failure: {error}"))
    }
}
// @cpt-end:cpt-cf-oagw-dod-proxy-timeout:p1:inst-upstream-invoke-fn-01

#[cfg(test)]
mod tests {
    use super::{build_target_url, enforce_plaintext_policy};
    use crate::domain::model::Scheme;

    // @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-http-disabled-test-01
    #[test]
    fn http_scheme_is_refused_when_plaintext_is_disabled() {
        let error = enforce_plaintext_policy(Scheme::Http, false)
            .expect_err("http must be refused when plaintext is disabled");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    // @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-http-disabled-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-ws-disabled-test-01
    #[test]
    fn ws_scheme_is_refused_the_same_way_as_http() {
        let http_error = enforce_plaintext_policy(Scheme::Http, false).unwrap_err();
        let ws_error = enforce_plaintext_policy(Scheme::Ws, false).unwrap_err();
        assert_eq!(http_error.status(), ws_error.status());
        assert_eq!(
            http_error.to_problem().problem_type,
            ws_error.to_problem().problem_type
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-plaintext-ws-disabled-test-01

    #[test]
    fn http_scheme_is_allowed_when_plaintext_is_enabled() {
        assert!(enforce_plaintext_policy(Scheme::Http, true).is_ok());
    }

    #[test]
    fn https_is_never_refused() {
        assert!(enforce_plaintext_policy(Scheme::Https, false).is_ok());
    }

    #[test]
    fn target_url_is_built_from_scheme_host_port_and_path() {
        let url = build_target_url(Scheme::Http, "api.example.com", 8080, "/v1/items?x=1")
            .expect("must build");
        assert_eq!(url.as_str(), "http://api.example.com:8080/v1/items?x=1");
    }

    #[test]
    fn a_non_http_scheme_is_rejected_as_a_protocol_error() {
        let error = build_target_url(Scheme::Wss, "api.example.com", 443, "/v1")
            .expect_err("wss cannot serve a plain HTTP proxy request");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    }
}
