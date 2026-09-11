//! The outbound HTTP leg.
//!
//! A single `hyper-util` client carries all three transport shapes the gateway
//! forwards: buffered exchanges, streamed responses (plain HTTP and SSE) and
//! WebSocket upgrades. It deliberately sits *below* the toolkit client stack:
//! a gateway must forward redirects, retries and content encodings verbatim
//! instead of acting on them, and it must not decompress a body it is relaying.
//!
//! Plaintext upstreams are refused here unless the gear was configured with
//! `allow_http_upstream`; `scheme: http` remains a legal configuration value
//! either way, because which schemes a configuration may name and whether a
//! plaintext connection may actually be opened are different questions.

use crate::domain::error::OagwError;
use crate::domain::services::proxy::{BodyStream, UpstreamTunnel};
use bytes::Bytes;
use futures_util::TryStreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri, Version};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::time::Duration;

/// The connector: TLS-capable (rustls with native roots) and plaintext-capable.
pub type OutboundConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

/// The raw upstream answer, before any gateway transformation.
pub struct UpstreamResponse {
    /// Upstream status.
    pub status: u16,
    /// Upstream HTTP version.
    pub version: Version,
    /// Upstream headers, in wire order.
    pub headers: Vec<(String, String)>,
    /// The body, streamed.
    pub body: BodyStream,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Dispatches requests to the configured upstream endpoints.
#[derive(Clone)]
pub struct OutboundClient {
    client: Client<OutboundConnector, Full<Bytes>>,
    timeout: Duration,
    allow_http: bool,
}

impl std::fmt::Debug for OutboundClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboundClient")
            .field("timeout", &self.timeout)
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl OutboundClient {
    /// Builds a client with the given response-head timeout and plaintext
    /// permission.
    ///
    #[must_use]
    pub fn new(timeout: Duration, allow_http: bool) -> Self {
        let mut builder = Client::builder(TokioExecutor::new());
        builder
            .pool_timer(TokioTimer::new())
            .http1_preserve_header_case(true)
            .http1_title_case_headers(false);
        Self {
            client: builder.build::<_, Full<Bytes>>(build_connector()),
            timeout,
            allow_http,
        }
    }

    /// The configured response-head timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Refuses a plaintext scheme unless the gear opted in.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::LinkUnavailable`] for `http`/`ws` when
    /// `allow_http_upstream` is off, and [`OagwError::ValidationError`] for a
    /// scheme that is neither the plaintext pair nor the TLS pair.
    pub fn check_scheme(&self, scheme: &str) -> Result<(), OagwError> {
        let normalized = scheme.trim_start_matches('/').to_ascii_lowercase();
        match normalized.as_str() {
            "http" | "ws" => {
                if self.allow_http {
                    Ok(())
                } else {
                    Err(OagwError::LinkUnavailable(format!(
                        "plaintext upstream scheme '{normalized}' is not permitted: \
                         allow_http_upstream is disabled"
                    )))
                }
            }
            "https" | "grpcs" | "wss" => Ok(()),
            other => Err(OagwError::ValidationError(format!(
                "upstream scheme '{other}' is not supported"
            ))),
        }
    }

    /// Sends a request and returns the upstream answer with a streamed body.
    ///
    /// The timeout bounds the response **head** only: a stream that keeps
    /// producing (an SSE feed) is never cut off by it.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ConnectionTimeout`] on the head timeout and
    /// [`OagwError::DownstreamError`] on a transport failure.
    pub async fn send(&self, request: Request<Full<Bytes>>) -> Result<UpstreamResponse, OagwError> {
        let head = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| {
                OagwError::ConnectionTimeout(format!(
                    "upstream did not answer within {}ms",
                    self.timeout.as_millis()
                ))
            })?
            .map_err(|error| transport_error(error, self.timeout))?;
        Ok(UpstreamResponse {
            status: head.status().as_u16(),
            version: head.version(),
            headers: collect_headers(head.headers()),
            body: Box::pin(
                head.into_body()
                    .into_data_stream()
                    .map_err(|error| std::io::Error::other(error.to_string())),
            ),
        })
    }

    /// Opens an upgraded tunnel (WebSocket) and returns the upstream socket.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::StreamAborted`] when the upstream declines the
    /// upgrade or the upgraded socket is never delivered.
    pub async fn connect(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<UpstreamTunnel, OagwError> {
        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| {
                OagwError::ConnectionTimeout(format!(
                    "upstream did not answer within {}ms",
                    self.timeout.as_millis()
                ))
            })?
            .map_err(|error| transport_error(error, self.timeout))?;
        let status = response.status().as_u16();
        let headers = collect_headers(response.headers());
        let upgraded = tokio::time::timeout(self.timeout, hyper::upgrade::on(response))
            .await
            .map_err(|_| {
                OagwError::StreamAborted("upstream never completed the upgrade".to_owned())
            })?
            .map_err(|error| {
                OagwError::StreamAborted(format!("upstream upgrade failed: {error}"))
            })?;
        Ok(UpstreamTunnel {
            status,
            headers,
            stream: Box::pin(super::websocket::UpgradedIo(upgraded)),
        })
    }
}

/// Builds an upstream request from its resolved parts.
///
/// # Errors
///
/// Returns [`OagwError::ValidationError`] when the method or the target URI
/// cannot be expressed on the wire.
#[allow(clippy::too_many_arguments)] // the eight parts of the wire request
pub fn build_request(
    method: &str,
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    query: Option<&str>,
    headers: &[(String, String)],
    body: Bytes,
) -> Result<Request<Full<Bytes>>, OagwError> {
    let verb = Method::from_bytes(method.as_bytes())
        .map_err(|_| OagwError::ValidationError(format!("method '{method}' is not valid")))?;
    // The default port stays off the wire; a non-default one is explicit.
    let authority = match default_port_for(scheme) {
        Some(default) if default == port => host.to_owned(),
        _ => format!("{host}:{port}"),
    };
    let path_and_query = match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_owned(),
    };
    let uri: Uri = format!(
        "{}://{}{}",
        normalized_scheme(scheme),
        authority,
        path_and_query
    )
    .parse()
    .map_err(|_| {
        OagwError::ValidationError(format!(
            "upstream target '{scheme}://{authority}{path}' is not a valid URI"
        ))
    })?;
    let mut builder = Request::builder()
        .method(verb)
        .version(Version::HTTP_11)
        .uri(uri);
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            OagwError::ValidationError(format!("request header name '{name}' is not valid"))
        })?;
        let value = HeaderValue::from_bytes(value.as_bytes()).map_err(|_| {
            OagwError::ValidationError(format!(
                "request header '{name}' has a value that is not valid"
            ))
        })?;
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(body))
        .map_err(|error| OagwError::ProtocolError(format!("outbound request: {error}")))
}

/// The parts of a transport failure worth reporting, with the timeout resolved
/// into its own variant.
// The timeout is a small `Copy` value read once, so owning it is the honest
// signature even though it is not moved from.
#[allow(clippy::needless_pass_by_value)]
fn transport_error(error: hyper_util::client::legacy::Error, timeout: Duration) -> OagwError {
    if is_head_timeout(&error) {
        return OagwError::RequestTimeout(format!(
            "upstream request timed out after {}ms",
            timeout.as_millis()
        ));
    }
    if is_connection_failure(&error) {
        return OagwError::LinkUnavailable(format!("upstream is unreachable: {error}"));
    }
    OagwError::DownstreamError(format!("upstream transport failed: {error}"))
}

/// Whether the transport failure is the client's own timeout.
///
/// `hyper-util`'s legacy error type exposes no timeout predicate, so the
/// classification reads the source chain for the pool/timeout markers it sets.
fn is_head_timeout(error: &hyper_util::client::legacy::Error) -> bool {
    let rendered = error.to_string();
    rendered.contains("timed out") || rendered.contains("timeout")
}

/// Whether the transport failure happened while establishing the connection.
fn is_connection_failure(error: &hyper_util::client::legacy::Error) -> bool {
    error.to_string().contains("connect")
}

/// Collects a header map into the ordered pair list the rest of the pipeline
/// works with.
#[must_use]
pub fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Adds a header to a builder, tolerating names the `http` crate cannot parse.
pub fn add_header(builder: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
        tracing::warn!(header = %name, "dropping header with an unparsable name");
        return;
    };
    let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) else {
        tracing::warn!(header = %name, "dropping header with an unparsable value");
        return;
    };
    builder.insert(name, value);
}

/// Lower-cases a scheme so lookups against the configured one are stable.
#[must_use]
pub fn normalized_scheme(scheme: &str) -> String {
    scheme.trim_start_matches('/').to_ascii_lowercase()
}

/// The port a scheme implies, when there is one.
#[must_use]
pub fn default_port_for(scheme: &str) -> Option<u16> {
    match normalized_scheme(scheme).as_str() {
        "http" | "ws" => Some(80),
        "https" | "wss" | "grpcs" => Some(443),
        _ => None,
    }
}

fn build_connector() -> OutboundConnector {
    let tls = match hyper_rustls::HttpsConnectorBuilder::new().with_native_roots() {
        Ok(builder) => builder,
        // No native store: fall back to the bundled Mozilla roots.
        Err(_) => hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots(),
    };
    tls.https_or_http().enable_all_versions().build()
}
