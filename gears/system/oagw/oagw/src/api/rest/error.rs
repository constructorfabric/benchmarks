//! Response rendering for OAGW errors and proxied responses.
//!
//! Gateway errors become RFC 9457 `application/problem+json` bodies with the
//! GTS `type` identifiers from DESIGN §3.3 and `X-OAGW-Error-Source: gateway`
//! (ADR-0007). The host's canonical-error middleware later fills `instance`
//! and `trace_id` on any `problem+json` response, so they are not set here.
//!
//! `DownstreamError` renders as a passthrough of the upstream body/headers
//! with `X-OAGW-Error-Source: upstream`. Proxied (`Ok`) responses are plain
//! passthroughs with the `X-OAGW-Error-Source: upstream` header the data
//! plane already attached.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::dto::ProxyResponse;
use crate::domain::error::{OagwError, RateLimitHeaders};

const PROBLEM_JSON: &str = "application/problem+json";
const ERROR_SOURCE: &str = "x-oagw-error-source";

/// RFC 9457 problem body with OAGW extension fields (DESIGN §3.3).
#[derive(Debug, Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    problem_type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin_id: Option<String>,
}

impl ProblemBody {
    fn from_error(err: &OagwError) -> Self {
        let (retry_after_seconds, plugin_id) = match err {
            OagwError::RateLimitExceeded { headers, .. } => {
                (headers.as_ref().map(|h| h.retry_after_secs), None)
            }
            OagwError::PluginInUse { plugin_id } => (None, Some(plugin_id.clone())),
            _ => (None, None),
        };
        Self {
            problem_type: err.gts_type(),
            title: err.title(),
            status: err.status().as_u16(),
            detail: err.detail(),
            retry_after_seconds,
            plugin_id,
        }
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        match &self {
            // Upstream error passthrough (ADR-0007).
            OagwError::DownstreamError {
                status,
                body,
                headers,
            } => {
                let status_code = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut builder = Response::builder().status(status_code);
                for (name, value) in headers {
                    // An upstream may send odd (non-ASCII) header bytes; skip
                    // rather than risk a panic on an untrusted value.
                    if let Ok(header_value) = HeaderValue::from_str(value) {
                        builder = builder.header(name.as_str(), header_value);
                    }
                }
                builder = builder.header(ERROR_SOURCE, "upstream");
                builder
                    .body(Body::from(body.clone()))
                    .expect("valid downstream error response")
            }
            _ => {
                let status = self.status();
                let builder = Response::builder().status(status);
                let builder = if let OagwError::RateLimitExceeded {
                    headers: Some(rl), ..
                } = &self
                {
                    append_rate_limit_headers(builder, rl)
                } else {
                    builder
                };
                let body = ProblemBody::from_error(&self);
                let bytes = serde_json::to_vec(&body).unwrap_or_default();
                builder
                    .header(header::CONTENT_TYPE, PROBLEM_JSON)
                    .header(ERROR_SOURCE, "gateway")
                    .body(Body::from(bytes))
                    .expect("valid problem response")
            }
        }
    }
}

/// Add rate-limit response headers (RFC 6585 / ADR-0003).
fn append_rate_limit_headers(
    builder: axum::http::response::Builder,
    rl: &RateLimitHeaders,
) -> axum::http::response::Builder {
    let mut builder = builder;
    for (name, value) in [
        ("x-ratelimit-limit", rl.limit.to_string()),
        ("x-ratelimit-remaining", rl.remaining.to_string()),
        ("x-ratelimit-reset", rl.reset_epoch_secs.to_string()),
        ("retry-after", rl.retry_after_secs.to_string()),
    ] {
        builder = builder.header(name, value.as_str());
    }
    builder
}

impl IntoResponse for ProxyResponse {
    fn into_response(self) -> Response {
        let mut builder = Response::builder().status(self.status);
        for (name, value) in &self.headers {
            // Skip unrepresentable header values rather than panic.
            if let Ok(header_value) = HeaderValue::from_str(value) {
                builder = builder.header(name.as_str(), header_value);
            }
        }
        builder
            .body(Body::from(self.body))
            .expect("valid proxy response")
    }
}
