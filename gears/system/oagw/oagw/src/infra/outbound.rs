//! Upstream forwarding via the toolkit HTTP client (DESIGN §3.2 "Upstream
//! Call").
//!
//! Raw request/response transfer: URL assembly, hop-by-hop header stripping,
//! `Host` replacement, deadline enforcement (via `tokio::time::timeout` +
//! client-side timeouts) and a hard body cap (100 MiB default from gear
//! config). All gateway-logic concerns (plugins, rate limiting, CORS, error
//! mapping) live in [`crate::domain::service`].

use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use toolkit_http::HttpClient;
use url::Url;

use crate::error::OagwError;
use crate::infra::ratelimit;

/// Hop-by-hop headers that must never be forwarded (RFC 9110 §7.6.1).
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// A fully-assembled outbound request.
pub struct OutboundRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

/// The upstream response, fully buffered (bounded by `max_body_bytes`).
pub struct OutboundResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Classified upstream failures for gateway error mapping.
pub enum ForwardError {
    /// Deadlines (connect/request/idle) → 504 family.
    Timeout,
    /// DNS, refused, reset, TLS, protocol breakage → 502.
    Transport,
    /// Upstream response body exceeded the hard cap → 413.
    BodyTooLarge,
    /// Anything else → 502 DownstreamError.
    Other(String),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardError::Timeout => write!(f, "upstream timeout"),
            ForwardError::Transport => write!(f, "upstream transport error"),
            ForwardError::BodyTooLarge => write!(f, "upstream response body too large"),
            ForwardError::Other(msg) => write!(f, "upstream error: {msg}"),
        }
    }
}

/// Sanitize a header map for upstream transmission: strip hop-by-hop headers
/// and replace `host` with the upstream authority.
#[must_use]
pub fn sanitize_outbound_headers(mut headers: HeaderMap, upstream_host: &str) -> HeaderMap {
    for key in HOP_BY_HOP {
        headers.remove(*key);
    }
    if let Ok(host) = HeaderValue::from_str(upstream_host) {
        headers.insert(axum::http::header::HOST, host);
    }
    headers
}

/// Assemble the outbound URL: scheme + host[:port] + path + query.
///
/// `path` must already carry the matched-route suffix (path_suffix_mode) and
/// `query_pairs` are appended over whatever the client supplied. Returns
/// `Err` for malformed input.
pub fn build_url(
    scheme: &str,
    host: &str,
    port: Option<u16>,
    path: &str,
    query_pairs: &[(String, String)],
) -> Result<String, String> {
    let mut url = Url::parse(&format!(
        "{}://{}:{}{}",
        scheme,
        host,
        port.unwrap_or(match scheme {
            "http" => 80,
            _ => 443,
        }),
        path
    ))
    .map_err(|e| format!("failed to build upstream URL: {e}"))?;
    for (k, v) in query_pairs {
        url.query_pairs_mut().append_pair(k, v);
    }
    Ok(url.to_string())
}

/// Forward a request to the upstream with deadline + body-limit enforcement.
pub async fn forward(
    client: &HttpClient,
    req: OutboundRequest,
    timeout: Duration,
    max_body_bytes: usize,
) -> Result<OutboundResponse, ForwardError> {
    let mut builder = match req.method.clone() {
        Method::GET => client.get(&req.url),
        Method::POST => client.post(&req.url),
        Method::PUT => client.put(&req.url),
        Method::PATCH => client.patch(&req.url),
        Method::DELETE => client.delete(&req.url),
        Method::HEAD => client.head(&req.url),
        Method::OPTIONS => client.options(&req.url),
        other => return Err(ForwardError::Other(format!("unsupported method {other}"))),
    };

    for (name, value) in req.headers.iter() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name.as_str(), value.to_str().unwrap_or(""));
        }
    }

    if let Some(body) = req.body {
        builder = builder.body_bytes(body);
    }
    // Only apply the hard cap on the response; per-request timeouts on the
    // client are configured at build time, and the outer tokio deadline is
    // the authoritative 504 source (keeps error classification stable).
    let resp = match tokio::time::timeout(timeout, builder.send()).await {
        Err(_) => return Err(ForwardError::Timeout),
        Ok(Err(e)) => return Err(classify(&e)),
        Ok(Ok(resp)) => resp,
    };

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = match tokio::time::timeout(timeout, resp.bytes()).await {
        Err(_) => return Err(ForwardError::Timeout),
        Ok(Err(e)) => match e {
            toolkit_http::HttpError::BodyTooLarge { .. } => {
                return Err(ForwardError::BodyTooLarge);
            }
            other => return Err(classify(&other)),
        },
        Ok(Ok(body)) => body,
    };

    if body.len() > max_body_bytes {
        return Err(ForwardError::BodyTooLarge);
    }

    Ok(OutboundResponse {
        status,
        headers,
        body,
    })
}

fn classify(error: &toolkit_http::HttpError) -> ForwardError {
    match error {
        toolkit_http::HttpError::Timeout(_) | toolkit_http::HttpError::DeadlineExceeded(_) => {
            ForwardError::Timeout
        }
        toolkit_http::HttpError::BodyTooLarge { .. } => ForwardError::BodyTooLarge,
        _ => ForwardError::Transport,
    }
}

/// Map a classified forward failure onto the OAGW error contract.
pub fn forward_error_to_oagw(error: ForwardError) -> OagwError {
    match error {
        ForwardError::Timeout => OagwError::RequestTimeout(error.to_string()),
        ForwardError::Transport => OagwError::DownstreamError(error.to_string()),
        ForwardError::BodyTooLarge => OagwError::PayloadTooLarge,
        ForwardError::Other(msg) => OagwError::DownstreamError(msg),
    }
}

/// Attach upstream-derived rate-limit response headers (pass-through of
/// upstream `X-RateLimit-*` when enabled).
#[must_use]
pub fn upstream_rate_limit_headers(
    decision: Option<&ratelimit::RateLimitDecision>,
) -> Vec<(String, String)> {
    let Some(d) = decision else {
        return Vec::new();
    };
    vec![
        ("X-RateLimit-Limit".to_owned(), d.limit.to_string()),
        ("X-RateLimit-Remaining".to_owned(), d.remaining.to_string()),
        ("X-RateLimit-Reset".to_owned(), d.reset_secs.to_string()),
    ]
}

/// Strip hop-by-hop headers from an upstream response before returning it to
/// the client.
#[must_use]
pub fn sanitize_inbound_headers(mut headers: HeaderMap) -> HeaderMap {
    for key in HOP_BY_HOP {
        headers.remove(*key);
    }
    headers
}

/// Convenience: coalesce a failing gateway response's status into a bool.
#[must_use]
pub fn is_server_error(status: StatusCode) -> bool {
    status.is_server_error()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_stripped_and_host_set() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("host", HeaderValue::from_static("client.example.com"));
        headers.insert("authorization", HeaderValue::from_static("Bearer x"));
        let sanitized = sanitize_outbound_headers(headers, "api.upstream.com:8443");
        assert!(sanitized.get("connection").is_none());
        assert!(sanitized.get("transfer-encoding").is_none());
        assert_eq!(
            sanitized.get("host").unwrap().to_str().unwrap(),
            "api.upstream.com:8443"
        );
        assert_eq!(
            sanitized.get("authorization").unwrap().to_str().unwrap(),
            "Bearer x"
        );
    }

    #[test]
    fn url_assembly_appends_query_and_default_port() {
        let url = build_url(
            "https",
            "api.openai.com",
            None,
            "/v1/models",
            &[("k".into(), "v".into())],
        )
        .unwrap();
        assert_eq!(url, "https://api.openai.com/v1/models?k=v");
    }

    #[test]
    fn url_assembly_preserves_explicit_port() {
        let url = build_url("http", "127.0.0.1", Some(9000), "/v1/chat", &[]).unwrap();
        assert_eq!(url, "http://127.0.0.1:9000/v1/chat");
    }

    #[test]
    fn inbound_sanitization_removes_hop_by_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", HeaderValue::from_static("h2c"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let sanitized = sanitize_inbound_headers(headers);
        assert!(sanitized.get("upgrade").is_none());
        assert!(sanitized.get("content-type").is_some());
    }
}
