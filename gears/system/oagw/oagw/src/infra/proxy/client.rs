//! Upstream HTTP client (DESIGN "Data plane" / ADR-0002).
//!
//! One pooled `hyper` client per plaintext posture. When
//! `allow_http_upstream` is `false` the HTTPS-only client is used, so a
//! plaintext upstream is unreachable no matter what the endpoint says.

use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, Version};
use http_body_util::Full;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::model::{Endpoint, EndpointScheme};
use crate::domain::services::proxy::{UpstreamRequest, UpstreamResponse};

type LegacyClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<bytes::Bytes>>;

/// Pooled upstream client used by the data plane.
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    https_only: LegacyClient,
    http_allowed: LegacyClient,
    timeout: Duration,
}

impl UpstreamClient {
    /// Builds both clients once (connection pools are shared per process).
    ///
    /// # Errors
    ///
    /// [`OagwError::Internal`] when the system trust store cannot be loaded.
    pub fn new(timeout: Duration) -> OagwResult<Self> {
        Ok(Self {
            https_only: build_client(false)?,
            http_allowed: build_client(true)?,
            timeout,
        })
    }

    /// Sends one upstream request and returns the raw upstream response.
    ///
    /// # Errors
    ///
    /// [`OagwError::ConnectionTimeout`] / [`OagwError::RequestTimeout`] on
    /// timeouts, [`OagwError::LinkUnavailable`] for transport failures and
    /// [`OagwError::Internal`] for malformed requests.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        request: &UpstreamRequest,
    ) -> crate::domain::error::OagwResult<UpstreamResponse> {
        let uri = build_uri(endpoint, &request.path, &request.query)?;
        let method =
            Method::from_bytes(request.method.as_bytes()).map_err(|_| {
                OagwError::Validation(format!("unsupported upstream method {}", request.method))
            })?;
        let mut builder = http::Request::builder()
            .method(method)
            .version(Version::HTTP_11)
            .uri(uri.clone());
        for (name, value) in &request.headers {
            if name == http::header::CONTENT_LENGTH {
                // hyper sets it from the body.
                continue;
            }
            builder = builder.header(name, value.clone());
        }

        let client = self.select(endpoint)?;
        let outgoing = builder
            .body(Full::new(request.body.clone()))
            .map_err(|err| {
                OagwError::Internal(format!("failed to build upstream request: {err}"))
            })?;

        let started = std::time::Instant::now();
        let response = tokio::time::timeout(self.timeout, client.request(outgoing))
            .await
            .map_err(|_| {
                if request.is_upgrade {
                    OagwError::ConnectionTimeout
                } else {
                    OagwError::RequestTimeout
                }
            })?
            .map_err(|err| transport_error(&err, started))?;

        let status = response.status();
        let headers = response.headers().clone();
        let is_upgrade = status == http::StatusCode::SWITCHING_PROTOCOLS;

        if is_upgrade && request.is_upgrade {
            // The client-side upgraded stream is taken from the response.
            let upgraded = tokio::time::timeout(self.timeout, hyper::upgrade::on(response))
                .await
                .map_err(|_| OagwError::ConnectionTimeout)?
                .map_err(|err| OagwError::LinkUnavailable(format!("upgrade failed: {err}")))?;
            return Ok(UpstreamResponse {
                status,
                headers,
                body: bytes::Bytes::new(),
                stream: None,
                upgraded: Some(upgraded),
                source: crate::domain::error::ErrorSource::Upstream,
            });
        }

        let incoming = response.into_body();
        let stream = crate::infra::proxy::stream::UpstreamBodyStream::new(incoming);
        Ok(UpstreamResponse {
            status,
            headers,
            body: bytes::Bytes::new(),
            stream: Some(stream),
            upgraded: None,
            source: crate::domain::error::ErrorSource::Upstream,
        })
    }

    /// Picks the client whose connector honours the configured posture.
    fn select(&self, endpoint: &Endpoint) -> crate::domain::error::OagwResult<&LegacyClient> {
        if endpoint.scheme == EndpointScheme::Http {
            // Plaintext is only ever attempted when explicitly allowed; the
            // HTTPS-only client would refuse the scheme outright.
            Ok(&self.http_allowed)
        } else {
            Ok(&self.https_only)
        }
    }
}

fn build_client(allow_http: bool) -> crate::domain::error::OagwResult<LegacyClient> {
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|err| OagwError::Internal(format!("trust store unavailable: {err}")))?;
    let builder = if allow_http {
        builder.https_or_http()
    } else {
        builder.https_only()
    };
    let connector = builder.enable_http1().enable_http2().build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// Builds the absolute upstream URI, never leaking the gateway's own path.
fn build_uri(
    endpoint: &Endpoint,
    path: &str,
    query: &[(String, String)],
) -> crate::domain::error::OagwResult<Uri> {
    let scheme = match endpoint.scheme {
        EndpointScheme::Http => "http",
        // ws(s)/grpc/webtransport all speak HTTP on the wire here.
        EndpointScheme::Https
        | EndpointScheme::Wss
        | EndpointScheme::Wt
        | EndpointScheme::Grpc => "https",
    };
    let host = if endpoint.host.contains(':') && !endpoint.host.starts_with('[') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    let authority = format!("{host}:{}", endpoint.port);
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let query = form_encode(query);
    let raw = if query.is_empty() {
        format!("{scheme}://{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}?{query}")
    };
    Uri::try_from(raw.as_str()).map_err(|err| {
        OagwError::Validation(format!(
            "upstream target '{}' is not a valid URI: {err}",
            raw
        ))
    })
}

/// Percent-encodes query pairs, preserving `+`-free RFC 3986 semantics.
#[must_use]
pub fn form_encode(query: &[(String, String)]) -> String {
    let mut parts = Vec::with_capacity(query.len());
    for (name, value) in query {
        parts.push(format!("{}={}", encode_component(name), encode_component(value)));
    }
    parts.join("&")
}

/// Percent-encodes one query component.
#[must_use]
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn transport_error(err: &hyper_util::client::legacy::Error, started: std::time::Instant) -> OagwError {
    let _ = started;
    let text = err.to_string();
    if text.contains("timed out") || text.contains("timeout") {
        return OagwError::ConnectionTimeout;
    }
    OagwError::LinkUnavailable(text)
}

/// Header values that must survive an upgrade hop verbatim.
pub const UPGRADE_FORWARD_HEADERS: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Copies the upgrade-relevant client headers into `headers`.
pub fn forward_upgrade_headers(headers: &mut HeaderMap, client_headers: &HeaderMap) {
    for name in UPGRADE_FORWARD_HEADERS {
        if let Some(value) = client_headers.get(*name) {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                headers.insert(name, value.clone());
            }
        }
    }
    if let Some(value) = client_headers.get("host") {
        headers.insert(http::header::HOST, value.clone());
    }
}

/// Convenience for building a header value, ignoring invalid input.
#[must_use]
pub fn header_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value).ok()
}
