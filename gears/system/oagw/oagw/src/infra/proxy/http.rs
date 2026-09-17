//! Outbound HTTP transport for the data plane.
//!
//! The gateway dials upstreams with a pooled `hyper-util` legacy client so a
//! response body can be streamed to the caller unbuffered and an upgrade can
//! be taken over the same connection.

use std::error::Error as _;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Uri};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, RequestHeaderRules, ResponseHeaderRules, Scheme};

/// Headers that never travel between the gateway and an upstream.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers the gateway owns: they are re-authored per request.
pub const GATEWAY_OWNED: [&str; 3] = ["host", "content-length", "x-oagw-target-host"];

/// Pooled outbound client shared by every proxied request.
#[derive(Clone)]
pub struct OutboundClient {
    inner: Client<hyper_rustls::HttpsConnector<HttpConnector>, axum::body::Body>,
}

impl OutboundClient {
    /// Build the client: TLS roots from the bundled webpki store, `http`
    /// allowed because the data plane gates plaintext itself.
    #[must_use]
    pub fn new() -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_all_versions()
            .build();
        let inner = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(64)
            .build(connector);
        Self { inner }
    }

    /// Dial the upstream and return once the response head has arrived.
    ///
    /// # Errors
    ///
    /// Returns `cf.oagw.timeout.request.v1` when the head does not arrive
    /// within `timeout`, `cf.oagw.link.unavailable.v1` when the connection
    /// itself fails and `cf.oagw.protocol.error.v1` for any other transport
    /// failure.
    pub async fn send(
        &self,
        request: axum::http::Request<axum::body::Body>,
        timeout: Duration,
    ) -> Result<axum::http::Response<Incoming>, DomainError> {
        let head = tokio::time::timeout(timeout, self.inner.request(request)).await;
        match head {
            Err(_) => Err(DomainError::request_timeout(format!(
                "upstream response head did not arrive within {timeout:?}"
            ))),
            Ok(Err(error)) => Err(transport_error(&error)),
            Ok(Ok(response)) => Ok(response),
        }
    }
}

impl Default for OutboundClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a transport failure onto the contract error catalog.
fn transport_error(error: &hyper_util::client::legacy::Error) -> DomainError {
    let detail = error
        .source()
        .map_or_else(|| error.to_string(), std::string::ToString::to_string);
    if error.is_connect() {
        return DomainError::link_unavailable(format!("upstream connection failed: {detail}"));
    }
    DomainError::protocol_error(format!("upstream transport failed: {detail}"))
}

/// URI scheme an endpoint is dialled with.
///
/// `wss` is HTTPS with an upgrade; `wt` and `grpc` have no dialable path in
/// this build and are refused before the connection is attempted.
#[must_use]
pub const fn dial_scheme(scheme: Scheme) -> Option<&'static str> {
    match scheme {
        Scheme::Http => Some("http"),
        Scheme::Https | Scheme::Wss => Some("https"),
        Scheme::Wt | Scheme::Grpc => None,
    }
}

/// Absolute URI for `endpoint` and the already-rewritten request `path`.
///
/// # Errors
///
/// Returns a validation error when the endpoint's scheme has no dialable
/// transport or the rendered URI is not parseable.
pub fn upstream_uri(
    endpoint: &Endpoint,
    path: &str,
    query: Option<&str>,
) -> Result<Uri, DomainError> {
    let Some(scheme) = dial_scheme(endpoint.scheme) else {
        return Err(DomainError::link_unavailable(format!(
            "upstream scheme `{:?}` has no dialable transport",
            endpoint.scheme
        )));
    };
    let authority = format!("{}:{}", endpoint.host, endpoint.port);
    let path_and_query = match query {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.to_owned(),
    };
    Uri::builder()
        .scheme(scheme)
        .authority(authority.as_str())
        .path_and_query(path_and_query.as_str())
        .build()
        .map_err(|error| {
            DomainError::validation(format!("upstream request URI is invalid: {error}"))
        })
}

/// Headers forwarded upstream: hop-by-hop stripped, `passthrough` applied.
///
/// The gateway never forwards an inbound header it has not been told to: the
/// default `passthrough: none` means only `set`/`add`/plugin-added headers
/// reach the upstream.
#[must_use]
pub fn outbound_headers(
    inbound: &HeaderMap,
    rules: &RequestHeaderRules,
    host: &str,
    upgrade: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in inbound {
        if forwardable(name, rules, upgrade) {
            headers.append(name.clone(), value.clone());
        }
    }
    apply_request_rules(&mut headers, rules);
    if let Ok(host) = HeaderValue::from_str(host) {
        headers.insert(axum::http::header::HOST, host);
    }
    headers
}

/// Apply the `set`/`add`/`remove` request rules to `headers`.
fn apply_request_rules(headers: &mut HeaderMap, rules: &RequestHeaderRules) {
    for (name, value) in &rules.set {
        set_header(headers, name, value);
    }
    for (name, value) in &rules.add {
        append_header(headers, name, value);
    }
    for name in &rules.remove {
        remove_header(headers, name);
    }
}

/// Whether `name` may be forwarded upstream.
fn forwardable(name: &HeaderName, rules: &RequestHeaderRules, upgrade: bool) -> bool {
    let lower = name.as_str().to_ascii_lowercase();
    if GATEWAY_OWNED.contains(&lower.as_str()) {
        return false;
    }
    if HOP_BY_HOP.contains(&lower.as_str()) && !(upgrade && lower == "upgrade") {
        return false;
    }
    if rules.set.contains_key(&lower) || rules.remove.contains(&lower) {
        return false;
    }
    // A WebSocket handshake is negotiated end to end, so its security headers
    // travel with the upgrade whatever `passthrough` says.
    if upgrade && (lower == "upgrade" || lower.starts_with("sec-websocket")) {
        return true;
    }
    match rules.passthrough {
        crate::domain::model::Passthrough::None => false,
        crate::domain::model::Passthrough::Allowlist => rules
            .passthrough_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&lower)),
        crate::domain::model::Passthrough::All => true,
    }
}

/// Headers handed back to the caller: hop-by-hop stripped, rules applied.
#[must_use]
pub fn response_headers(upstream: &HeaderMap, rules: &ResponseHeaderRules) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        let lower = name.as_str().to_ascii_lowercase();
        let dropped = HOP_BY_HOP.contains(&lower.as_str())
            || lower == "content-length"
            || rules
                .remove
                .iter()
                .any(|drop| drop.eq_ignore_ascii_case(&lower));
        if !dropped {
            headers.append(name.clone(), value.clone());
        }
    }
    for (name, value) in &rules.set {
        set_header(&mut headers, name, value);
    }
    for (name, value) in &rules.add {
        append_header(&mut headers, name, value);
    }
    for name in &rules.remove {
        remove_header(&mut headers, name);
    }
    headers
}

/// Insert or overwrite a header, ignoring an unparsable name or value.
fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = HeaderName::try_from(name)
        && let Ok(value) = HeaderValue::from_str(value)
    {
        headers.insert(name, value);
    }
}

/// Append a header, ignoring an unparsable name or value.
fn append_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = HeaderName::try_from(name)
        && let Ok(value) = HeaderValue::from_str(value)
    {
        headers.append(name, value);
    }
}

/// Drop every value of a header, ignoring an unparsable name.
fn remove_header(headers: &mut HeaderMap, name: &str) {
    if let Ok(name) = HeaderName::try_from(name) {
        headers.remove(name);
    }
}
