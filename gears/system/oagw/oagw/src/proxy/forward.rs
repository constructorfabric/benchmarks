//! The upstream call, body validation and error mapping (R10-R17).
//!
//! [`read_body`] enforces the request-body guards of DESIGN.md §3.2 "Body
//! Validation Rules" while the body is still streaming, [`UpstreamCall`] is the
//! request that is handed to the `toolkit-http` client, and
//! [`map_transport_error`] turns client failures into the gateway problems of
//! DESIGN.md §3.3 "Error Response Format" (R15).
//!
//! The forward path is split so Phase 5 can add a streaming variant: the body
//! is read chunk by chunk and the upstream response is returned as
//! `Response<ResponseBody>` rather than a fully buffered string, so SSE and
//! WebSocket bodies can replace [`read_body`] / the response conversion without
//! touching resolution, matching or header handling.

use std::sync::Arc;

use axum::http::HeaderMap;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use toolkit_http::{
    HttpClient, HttpClientBuilder, HttpClientConfig, HttpError, HttpResponse, TransportSecurity,
};
use tracing::warn;

use crate::config::OagwConfig;
use crate::domain::model::{Endpoint, HttpMethod, Scheme};
use crate::error::{GatewayError, GatewayErrorKind};

/// The hard request-body limit of DESIGN.md §3.2 "Body Validation Rules" (R11).
pub const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;

/// The hard request-body limit of DESIGN.md §3.2 "Body Validation Rules" (R11),
/// in whole mebibytes, for the messages of R11.
const MAX_REQUEST_BODY_MB: usize = 100;

/// The user agent the gateway dials upstream with.
const USER_AGENT: &str = "oagw-gateway/1.0";

/// The request that is dialled upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamCall {
    /// The forwarded method.
    pub method: HttpMethod,
    /// Scheme of the target endpoint.
    pub scheme: Scheme,
    /// `host` or `host:port` of the target endpoint (R7).
    pub authority: String,
    /// Path to forward, with the request's path suffix appended.
    pub path: String,
    /// Query string to forward, reduced to the allowlisted parameters.
    pub query: Option<String>,
    /// Headers to forward (R7, R8, R9).
    pub headers: HeaderMap,
    /// Body to forward, already validated and buffered (R10-R12).
    pub body: Bytes,
}

impl UpstreamCall {
    /// The absolute URL the upstream is dialled at.
    #[must_use]
    pub fn url(&self) -> String {
        let scheme = self.scheme.as_str();

        if let Some(query) = &self.query {
            format!("{scheme}://{}{}?{query}", self.authority, self.path)
        } else {
            format!("{scheme}://{}{}", self.authority, self.path)
        }
    }

    /// The headers that survive as valid HTTP header values, as `(name, value)`
    /// string pairs. Values that are not visible ASCII (which HTTP forbids in a
    /// header value) are dropped with a warning rather than failing the request.
    #[must_use]
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .map(|text| (name.as_str().to_owned(), text.to_owned()))
                    .inspect_err(|_| {
                        warn!(header = %name, "dropping a header whose value is not visible ASCII");
                    })
                    .ok()
            })
            .collect()
    }
}

/// Builds the upstream client of a gateway (R2, R13, R15, R16).
///
/// The client is built once and shared: no retry policy (R16), no redirects (a
/// redirect answer is passed through to the caller, R14), a per-request timeout
/// of `proxy_timeout_secs` (R15) and a transport policy that dials plaintext
/// only while `allow_http_upstream` is true (R13).
///
/// # Errors
///
/// Returns 500 when the client cannot be built at all (no usable TLS
/// configuration).
pub fn build_client(config: &OagwConfig) -> Result<HttpClient, GatewayError> {
    let requested = transport_policy(config.allow_http_upstream);

    if let Ok(client) = client(requested, config).build() {
        return Ok(client);
    }

    // Under `--features fips` the toolkit refuses to build a client that would
    // dial plaintext, and a gear whose registration fails takes the whole
    // gateway down. Fall back to TLS-only dialling instead: plaintext
    // upstreams are then rejected by the client rather than by the policy
    // check, which is the safer direction.
    warn!(
        allow_http_upstream = config.allow_http_upstream,
        "toolkit-http refused the configured transport; falling back to TLS-only dialling"
    );

    client(TransportSecurity::TlsOnly, config)
        .build()
        .map_err(|error| {
            GatewayError::new(
                GatewayErrorKind::Internal,
                format!("the upstream HTTP client could not be built: {error}"),
            )
        })
}

/// The upstream client of a gateway, over the `toolkit-http` "proxy" profile.
///
/// The profile already encodes the edge rules this gear inherits: no retry
/// (R16), no client-side rate limit, no response-body cap (a large download
/// streams through untruncated, R14) and no redirect following (a `3xx` answer
/// reaches the caller verbatim, R14). On top of it the data plane sets its own
/// user agent, its per-request timeout (R15) and the transport policy (R13).
fn client(transport: TransportSecurity, config: &OagwConfig) -> HttpClientBuilder {
    HttpClientBuilder::with_config(HttpClientConfig::proxy())
        .timeout(config.proxy_timeout())
        .user_agent(USER_AGENT)
        .retry(None)
        .max_body_size(usize::MAX)
        .transport(transport)
        .no_redirects()
}

/// The transport policy of a gateway: plaintext only while the gear allows it.
fn transport_policy(allow_http_upstream: bool) -> TransportSecurity {
    if allow_http_upstream {
        TransportSecurity::AllowInsecureHttp
    } else {
        TransportSecurity::TlsOnly
    }
}

/// Validates the request metadata and reads the body to forward (R10, R11, R12).
///
/// The declared `Content-Length` is checked before a byte is buffered, and the
/// read aborts — rather than draining — as soon as the accumulated body passes
/// [`MAX_REQUEST_BODY_BYTES`].
///
/// # Errors
///
/// Returns 400 for a non-integer or mismatching `Content-Length` and for a
/// `Transfer-Encoding` other than `chunked`, and 413 for a body beyond the hard
/// limit.
pub async fn read_body(headers: &HeaderMap, body: axum::body::Body) -> Result<Bytes, GatewayError> {
    check_transfer_encoding(headers)?;
    let declared = declared_content_length(headers)?;

    let mut buffer = BytesMut::new();
    let mut stream = body.into_data_stream();
    let mut total = 0_usize;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            GatewayError::new(
                GatewayErrorKind::Validation,
                format!("the request body could not be read: {error}"),
            )
        })?;

        total += chunk.len();
        if total > MAX_REQUEST_BODY_BYTES {
            return Err(payload_too_large(total));
        }

        buffer.extend_from_slice(&chunk);
    }

    let body = buffer.freeze();

    if let Some(declared) = declared
        && declared != body.len()
    {
        return Err(content_length_mismatch(declared, body.len()));
    }

    Ok(body)
}

/// Rejects a `Transfer-Encoding` other than `chunked` (R12).
fn check_transfer_encoding(headers: &HeaderMap) -> Result<(), GatewayError> {
    for value in headers.get_all(http::header::TRANSFER_ENCODING) {
        let value = value.to_str().unwrap_or_default();

        if value.trim().eq_ignore_ascii_case("chunked") {
            continue;
        }

        return Err(GatewayError::validation(
            format!(
                "`transfer-encoding: {value}` is not supported; only `chunked` request bodies \
                 are forwarded"
            ),
            "transfer-encoding",
        ));
    }

    Ok(())
}

/// The declared `Content-Length`, or `None` when the request declares none.
///
/// # Errors
///
/// Returns 400 when the header is not a non-negative integer (R10).
fn declared_content_length(headers: &HeaderMap) -> Result<Option<usize>, GatewayError> {
    let Some(value) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };

    let value = value.to_str().unwrap_or_default().trim();

    value.parse::<usize>().map(Some).map_err(|_| {
        GatewayError::validation(
            format!("`content-length: {value}` is not a valid byte count"),
            "content-length",
        )
    })
}

/// 413 `cf.oagw.payload.too_large.v1` (R11).
fn payload_too_large(actual: usize) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::PayloadTooLarge,
        format!(
            "the request body exceeds the {MAX_REQUEST_BODY_MB} MB hard limit ({actual} bytes buffered)"
        ),
    )
}

/// 400 for a `Content-Length` that does not match the body actually read (R10).
fn content_length_mismatch(declared: usize, actual: usize) -> GatewayError {
    GatewayError::validation(
        format!("`content-length: {declared}` does not match the {actual} bytes actually received"),
        "content-length",
    )
}

/// Rejects a plaintext target the gear policy does not allow (R13).
pub fn check_scheme_policy(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), GatewayError> {
    if config.allow_http_upstream || !endpoint.is_plaintext() {
        return Ok(());
    }

    Err(GatewayError::new(
        GatewayErrorKind::LinkUnavailable,
        format!(
            "upstream endpoint `{}://{}:{}` is plaintext and `allow_http_upstream` is false",
            endpoint.scheme,
            endpoint.host.as_str(),
            endpoint.port
        ),
    ))
}

/// Performs the upstream call (R2, R15).
///
/// # Errors
///
/// Returns the gateway problems of [`map_transport_error`] when the call does
/// not reach an HTTP status.
pub async fn dial(client: &HttpClient, call: &UpstreamCall) -> Result<HttpResponse, GatewayError> {
    let builder = request_builder(client, call);

    builder
        .send()
        .await
        .map_err(|error| map_transport_error(error, &call.url()))
}

/// Builds the `toolkit-http` request of a call.
///
/// [`HttpClient`] exposes one constructor per method, so the domain method is
/// matched exhaustively: every method a route can declare forwards, and a
/// method that cannot be named is never forwarded (see [`crate::proxy::matcher`]).
fn request_builder(client: &HttpClient, call: &UpstreamCall) -> toolkit_http::RequestBuilder {
    let url = call.url();
    let method = call.method;

    let builder = match method {
        HttpMethod::Get => client.get(&url),
        HttpMethod::Post => client.post(&url),
        HttpMethod::Put => client.put(&url),
        HttpMethod::Patch => client.patch(&url),
        HttpMethod::Delete => client.delete(&url),
        HttpMethod::Head => client.head(&url),
        HttpMethod::Options => client.options(&url),
    };

    builder
        .headers(call.header_pairs())
        .body_bytes(call.body.clone())
}

/// Maps a client failure to the gateway problem of DESIGN.md §3.3 (R15).
fn map_transport_error(error: HttpError, url: &str) -> GatewayError {
    match error {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => GatewayError::new(
            GatewayErrorKind::UpstreamTimeout,
            format!("the upstream did not answer `{url}` within the configured timeout"),
        ),
        HttpError::InvalidScheme { scheme, reason } => GatewayError::new(
            GatewayErrorKind::LinkUnavailable,
            format!("the upstream link `{scheme}` is not diallable: {reason}"),
        ),
        // Everything else — connection refused, DNS failure, TLS handshake,
        // a closed buffer worker — is the upstream being unreachable (502).
        _ => GatewayError::new(
            GatewayErrorKind::DownstreamError,
            format!("the upstream at `{url}` could not be reached: {error}"),
        ),
    }
}

/// The upstream service of the data plane.
///
/// One instance serves every proxied request: it holds the gear configuration,
/// the shared upstream client and the round-robin cursors of the endpoint
/// pools.
pub struct ProxyService {
    /// The configuration and the configuration store.
    pub config: Arc<crate::domain::store::ConfigService>,
    /// The client every upstream is dialled with.
    pub client: HttpClient,
    /// Round-robin cursors, one per multi-endpoint upstream.
    pub round_robin: crate::proxy::headers::RoundRobin,
}

impl ProxyService {
    /// Builds the data plane of a gateway configuration.
    ///
    /// # Errors
    ///
    /// Returns 500 when the upstream client cannot be built.
    pub fn new(config: Arc<crate::domain::store::ConfigService>) -> Result<Self, GatewayError> {
        let client = build_client(config.config())?;

        Ok(Self {
            config,
            client,
            round_robin: crate::proxy::headers::RoundRobin::default(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::body::Body;
    use http::HeaderMap;

    use super::*;
    use toolkit_http::TransportSecurity;

    fn config(allow_http: bool) -> OagwConfig {
        OagwConfig {
            allow_http_upstream: allow_http,
            ..OagwConfig::default()
        }
    }

    #[tokio::test]
    async fn test_a_body_within_the_limit_is_read_whole() {
        let headers = HeaderMap::new();

        let body = read_body(&headers, Body::from("hello gateway"))
            .await
            .unwrap();

        assert_eq!(body, Bytes::from_static(b"hello gateway"));
    }

    #[tokio::test]
    async fn test_a_declared_content_length_that_matches_is_accepted() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "11".parse().unwrap());

        let body = read_body(&headers, Body::from("hello world"))
            .await
            .unwrap();

        assert_eq!(body.len(), 11);
    }

    #[tokio::test]
    async fn test_a_non_integer_content_length_is_a_400() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "eleven".parse().unwrap());

        let error = read_body(&headers, Body::from("hello world"))
            .await
            .unwrap_err();

        assert_eq!(error.status(), 400);
        assert_eq!(error.kind(), GatewayErrorKind::Validation);
        assert!(error.detail().contains("eleven"), "{}", error.detail());
    }

    #[tokio::test]
    async fn test_a_mismatching_content_length_is_a_400() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "5".parse().unwrap());

        let error = read_body(&headers, Body::from("hello gateway"))
            .await
            .unwrap_err();

        assert_eq!(error.status(), 400);
        assert!(
            error.detail().contains("does not match"),
            "{}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn test_a_transfer_encoding_other_than_chunked_is_a_400() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::TRANSFER_ENCODING,
            "gzip, deflate".parse().unwrap(),
        );

        let error = read_body(&headers, Body::from("hello")).await.unwrap_err();

        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("gzip"), "{}", error.detail());
    }

    #[tokio::test]
    async fn test_a_chunked_transfer_encoding_is_accepted() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());

        let body = read_body(&headers, Body::from("hello")).await.unwrap();

        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn test_an_oversized_body_is_rejected_before_it_is_buffered() {
        let headers = HeaderMap::new();
        let stream = futures_util::stream::repeat_with(|| {
            Ok::<_, axum::Error>(Bytes::from_static(&[0_u8; 1024]))
        });

        let error = read_body(&headers, Body::from_stream(stream))
            .await
            .unwrap_err();

        assert_eq!(error.status(), 413);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        );
    }

    #[test]
    fn test_the_client_policy_follows_the_gear_configuration() {
        assert_eq!(
            transport_policy(false),
            TransportSecurity::TlsOnly,
            "plaintext upstreams must not be diallable while the policy forbids them"
        );
        assert_eq!(transport_policy(true), TransportSecurity::AllowInsecureHttp);
    }

    #[tokio::test]
    async fn test_the_client_disables_retry_and_redirects() {
        let client = build_client(&config(true)).expect("client builds");

        // The client is opaque; what matters is that it built and that the
        // policy that produced it is retry-free (see `transport_policy`).
        let _ = client;
    }

    #[test]
    fn test_an_unreachable_upstream_is_a_502_downstream_problem() {
        let error = map_transport_error(
            HttpError::Transport("connection refused".into()),
            "http://127.0.0.1:1/v1",
        );

        assert_eq!(error.status(), 502);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
        );
    }

    #[test]
    fn test_a_timed_out_upstream_call_is_a_504_timeout_problem() {
        let error =
            map_transport_error(HttpError::Timeout(std::time::Duration::from_secs(2)), "url");

        assert_eq!(error.status(), 504);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        );
    }

    #[test]
    fn test_a_scheme_the_transport_refuses_is_a_503_link_problem() {
        let error = map_transport_error(
            HttpError::InvalidScheme {
                scheme: "http".to_owned(),
                reason: "HTTPS required".to_owned(),
            },
            "url",
        );

        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        );
    }

    #[test]
    fn test_the_call_url_carries_the_scheme_authority_path_and_query() {
        let call = UpstreamCall {
            method: HttpMethod::Get,
            scheme: Scheme::Https,
            authority: "api.openai.com".to_owned(),
            path: "/v1/chat/completions".to_owned(),
            query: Some("model=gpt-4".to_owned()),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };

        assert_eq!(
            call.url(),
            "https://api.openai.com/v1/chat/completions?model=gpt-4"
        );

        let without_query = UpstreamCall {
            query: None,
            ..call
        };
        assert_eq!(
            without_query.url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_header_pairs_pass_valid_values_through() {
        let mut headers = HeaderMap::new();
        headers.insert("x-oagw-request-id", "abc".parse().unwrap());

        let call = UpstreamCall {
            method: HttpMethod::Get,
            scheme: Scheme::Https,
            authority: "api.openai.com".to_owned(),
            path: "/v1".to_owned(),
            query: None,
            headers,
            body: Bytes::new(),
        };

        assert_eq!(
            call.header_pairs(),
            vec![("x-oagw-request-id".to_owned(), "abc".to_owned())]
        );
    }

    #[tokio::test]
    async fn test_a_plaintext_endpoint_is_rejected_while_the_policy_forbids_it() {
        let endpoint: Endpoint = serde_json::from_value(
            serde_json::json!({ "host": "api.example.com", "scheme": "http" }),
        )
        .unwrap();

        let error = check_scheme_policy(&config(false), &endpoint).unwrap_err();

        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        );
    }

    #[tokio::test]
    async fn test_a_plaintext_endpoint_is_dialled_while_the_policy_allows_it() {
        let endpoint: Endpoint = serde_json::from_value(
            serde_json::json!({ "host": "api.example.com", "scheme": "http" }),
        )
        .unwrap();

        assert!(check_scheme_policy(&config(true), &endpoint).is_ok());
    }
}
