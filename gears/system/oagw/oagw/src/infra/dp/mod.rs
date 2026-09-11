// Created: 2026-09-01 by Constructor Tech
//! The Data Plane.
//!
//! `docs/DESIGN.md` §4: take one request, enforce the gateway's own policy,
//! open the upstream, and hand back either a response or a negotiated
//! upgrade. Everything the request needs from the Control Plane — the
//! effective upstream, the merged plugin chain, the merged rate-limit
//! policy — has been resolved before it arrives; this layer only executes
//! and keeps the runtime state that outlives a request: the rate-limit
//! counters and the per-endpoint circuit breakers.

pub mod request;
pub mod ws;

pub use ws::{UpstreamHead, UpstreamIo, connect as connect_ws, relay};

use std::sync::Arc;
use std::time::Duration;

use toolkit_http::{HttpClient, HttpClientBuilder, HttpClientConfig, TransportSecurity};

use crate::domain::breaker::{CircuitBreaker, Thresholds, Verdict};
use crate::domain::errors::OagwError;
use crate::domain::model::{HeadersConfig, RateLimit, Target};
use crate::domain::ratelimit::{CounterKey, Decision, RateLimiter};
use crate::infra::context::{PluginRequest, ProxyBody, ProxyOutcome, ProxyResponse, UpgradeHandle};

/// Data-plane runtime knobs, derived from `OagwConfig` at boot.
#[derive(Debug, Clone)]
pub struct DpConfig {
    /// Time-to-first-byte budget. A stream that has already started is never
    /// cut off by this, so an SSE session outlives it comfortably.
    pub ttfb: Duration,
    /// Whether a plaintext upstream connection may actually be made.
    pub allow_http_upstream: bool,
    /// Largest accepted inbound request body.
    pub max_body_size: usize,
    /// Circuit-breaker thresholds.
    pub breaker: Thresholds,
    /// How long a quiet WebSocket session is kept before it is dropped.
    pub ws_idle_timeout: Duration,
}

/// Executes proxy requests against the upstreams.
pub struct DataPlane {
    http: HttpClient,
    ws_tls: Option<Arc<rustls::ClientConfig>>,
    cfg: DpConfig,
    limiter: RateLimiter,
    breaker: CircuitBreaker,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlane")
            .field("allow_http_upstream", &self.cfg.allow_http_upstream)
            .field("ttfb_secs", &self.cfg.ttfb.as_secs())
            .field("max_body_size", &self.cfg.max_body_size)
            .finish()
    }
}

impl DataPlane {
    /// Build a data plane.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be constructed at all.
    pub fn new(cfg: DpConfig) -> Result<Self, OagwError> {
        let allow_http_upstream = cfg.allow_http_upstream;
        let transport = if allow_http_upstream {
            TransportSecurity::AllowInsecureHttp
        } else {
            TransportSecurity::TlsOnly
        };
        // `proxy()` already turns off retries, client-side rate limiting,
        // redirect following and the response-body cap. Only the transport
        // needs overriding, because that is what the caller configures.
        let http = match HttpClientBuilder::with_config(HttpClientConfig::proxy())
            .transport(transport)
            .build()
        {
            Ok(client) => client,
            // FIPS refuses the plaintext transport outright. Rather than
            // refuse to start, refuse the plaintext *connections* — the
            // flag stays set and every non-secure target is rejected later,
            // which is what FIPS mandates and no more than that.
            Err(error) if allow_http_upstream => {
                tracing::warn!(
                    error = %error,
                    "plaintext upstreams are unavailable in this build; falling back to TLS-only"
                );
                HttpClientBuilder::with_config(HttpClientConfig::proxy())
                    .transport(TransportSecurity::TlsOnly)
                    .build()
                    .map_err(|error| {
                        OagwError::link_unavailable(format!("http client unavailable: {error}"))
                    })?
            }
            Err(error) => {
                return Err(OagwError::link_unavailable(format!(
                    "http client unavailable: {error}"
                )));
            }
        };
        let breaker = CircuitBreaker::new(cfg.breaker);
        Ok(Self {
            http,
            ws_tls: ws_tls_config(),
            cfg,
            limiter: RateLimiter::new(),
            breaker,
        })
    }

    /// The HTTP client used for ordinary forwards.
    #[must_use]
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// The rate limiter backing the `rate_limit` blocks.
    #[must_use]
    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// The circuit breaker tracking upstream health.
    #[must_use]
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// `true` when a plaintext upstream connection is permitted.
    #[must_use]
    pub fn allows_http(&self) -> bool {
        self.cfg.allow_http_upstream
    }

    /// The inbound-body ceiling.
    #[must_use]
    pub fn max_body_size(&self) -> usize {
        self.cfg.max_body_size
    }

    /// The idle timeout applied to a relayed WebSocket session.
    #[must_use]
    pub fn ws_idle_timeout(&self) -> Duration {
        self.cfg.ws_idle_timeout
    }

    /// Enforce the rate limit, when the effective configuration has one.
    ///
    /// `None` means unbounded and is never a rejection. A `Some` decision
    /// describes the consumption that just happened, so the caller can report
    /// it back in the `X-RateLimit-*` headers.
    ///
    /// # Errors
    /// Returns `429 RateLimitExceeded` with `Retry-After` guidance.
    pub fn check_rate(
        &self,
        config: Option<&RateLimit>,
        request: &PluginRequest,
    ) -> Result<Option<Decision>, OagwError> {
        let Some(limit) = config else {
            return Ok(None);
        };
        let key = CounterKey::build(
            limit.scope,
            &request.tenant_id,
            request.subject.as_deref().unwrap_or("anonymous"),
            request.route_id.as_deref().unwrap_or_default(),
        );
        let decision = self.limiter.check(&key, limit);
        if decision.allowed {
            return Ok(Some(decision));
        }
        Err(rejected(&decision))
    }

    /// Refuse to dispatch when the breaker for this endpoint is open.
    ///
    /// # Errors
    /// Returns `503 CircuitBreakerOpen` with `Retry-After` guidance.
    pub fn check_breaker(&self, target: &Target) -> Result<(), OagwError> {
        match self.breaker.probe(&endpoint_key(target)) {
            Verdict::Closed => Ok(()),
            Verdict::Open { retry_after_secs } => {
                Err(OagwError::circuit_breaker_open(retry_after_secs))
            }
        }
    }

    /// Refuse to write plaintext when the flag forbids it.
    ///
    /// # Errors
    /// Returns `503 LinkUnavailable` naming the offending authority.
    pub fn check_transport(&self, target: &Target) -> Result<(), OagwError> {
        if target.secure || self.cfg.allow_http_upstream {
            return Ok(());
        }
        Err(OagwError::link_unavailable(format!(
            "plaintext upstream '{}' is refused because allow_http_upstream is disabled",
            target.authority()
        )))
    }

    /// Record the outcome of a dispatch against this endpoint.
    ///
    /// Only retriable failures count against the breaker: a `404` says
    /// nothing about whether the endpoint can be reached.
    pub fn record(&self, target: &Target, outcome: Option<&OagwError>) {
        let key = endpoint_key(target);
        match outcome {
            None => self.breaker.record_success(&key),
            Some(error) if error.kind().retriable() => self.breaker.record_failure(&key),
            Some(_) => {}
        }
    }

    /// Build the outbound URL for `target` and `path`.
    #[must_use]
    pub fn upstream_url(&self, target: &Target, path: &str) -> String {
        let scheme = if target.secure { "https" } else { "http" };
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        format!("{scheme}://{}{path}", target.authority())
    }

    /// Forward a request, streaming the response body straight through.
    ///
    /// The time-to-first-byte budget bounds the wait for the response head
    /// only; once the head has arrived the body streams for as long as the
    /// upstream keeps it open.
    ///
    /// # Errors
    /// Propagates transport failures.
    pub async fn send(
        &self,
        method: &str,
        request: &PluginRequest,
        headers: Option<&HeadersConfig>,
    ) -> Result<ProxyResponse, OagwError> {
        let url = self.upstream_url(&request.target, &request.path);
        let builder = request::builder(&self.http, method, &url, &request.headers, &request.body)?;
        let response = tokio::time::timeout(self.cfg.ttfb, builder.send())
            .await
            .map_err(|_| OagwError::request_timeout())?
            .map_err(map_transport)?;
        Ok(render(response, headers))
    }

    /// Negotiate a WebSocket session with the upstream.
    ///
    /// The request head must already be final: the caller has run the plugin
    /// chain and folded in the gateway's own additions. Whatever the client
    /// put in `Sec-WebSocket-Key` reaches the upstream untouched.
    ///
    /// # Errors
    /// Propagates transport failures.
    pub async fn open_websocket(
        &self,
        request: &PluginRequest,
        headers: Option<&HeadersConfig>,
    ) -> Result<ProxyOutcome, OagwError> {
        let head = request::upgrade_head(
            &request.method,
            &self.upstream_url(&request.target, &request.path),
            &request.headers,
            &request.target,
            headers,
        )?;
        let (mut io, upstream) = connect_ws(&request.target, &head, self.ws_tls.as_ref()).await?;
        if upstream.status != 101 {
            // The upstream declined. Its answer is an ordinary HTTP
            // response and the socket carries nothing further of interest.
            let _ = io.shutdown().await;
            return Ok(ProxyOutcome::Response(ProxyResponse {
                status: upstream.status,
                headers: crate::domain::headers::flatten(
                    &crate::domain::headers::transform_response(
                        &head_map(&upstream.headers),
                        headers,
                    ),
                ),
                body: ProxyBody::Full(bytes::Bytes::new()),
            }));
        }
        Ok(ProxyOutcome::Upgraded(UpgradeHandle {
            status: upstream.status,
            headers: upstream.headers,
            io,
        }))
    }
}

fn head_map(pairs: &[(String, String)]) -> http::HeaderMap {
    let mut map = http::HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            map.append(name, value);
        }
    }
    map
}

fn endpoint_key(target: &Target) -> String {
    format!("{}:{}", target.host, target.port)
}

fn rejected(decision: &Decision) -> OagwError {
    OagwError::rate_limit_exceeded(decision.retry_after_secs)
        .with_extension("x-ratelimit-limit", serde_json::json!(decision.limit))
        .with_extension("x-ratelimit-remaining", serde_json::json!(0))
        .with_extension("x-ratelimit-reset", serde_json::json!(decision.reset_secs))
}

/// The `X-RateLimit-*` headers reporting `decision` (`docs/ADR/0003`).
#[must_use]
pub fn rate_limit_headers(decision: &Decision) -> Vec<(&'static str, String)> {
    vec![
        ("x-ratelimit-limit", decision.limit.to_string()),
        ("x-ratelimit-remaining", decision.remaining.to_string()),
        ("x-ratelimit-reset", decision.reset_secs.to_string()),
    ]
}

fn map_transport(error: toolkit_http::HttpError) -> OagwError {
    match &error {
        toolkit_http::HttpError::Timeout(_) | toolkit_http::HttpError::DeadlineExceeded(_) => {
            OagwError::request_timeout()
        }
        toolkit_http::HttpError::InvalidScheme { scheme, .. } => OagwError::link_unavailable(
            format!("the upstream scheme '{scheme}' is not permitted: {error}"),
        ),
        toolkit_http::HttpError::Tls(_) => {
            OagwError::link_unavailable(format!("the TLS handshake failed: {error}"))
        }
        toolkit_http::HttpError::InvalidUri { .. } => {
            OagwError::validation_error(format!("the upstream URL is not usable: {error}"))
        }
        _ => OagwError::downstream_error(format!("the upstream request failed: {error}")),
    }
}

/// Turn an upstream reply into what the client receives.
///
/// The body is always streamed: a reverse proxy has no business buffering an
/// unbounded response in memory, and a finite one streams through just as
/// well as an infinite one.
fn render(response: toolkit_http::HttpResponse, headers: Option<&HeadersConfig>) -> ProxyResponse {
    let status = response.status().as_u16();
    let upstream = response.headers().clone();
    let body = response.into_body();
    ProxyResponse {
        status,
        headers: crate::domain::headers::flatten(&crate::domain::headers::transform_response(
            &upstream, headers,
        )),
        body: ProxyBody::Stream(body),
    }
}

/// The TLS trust anchors for the WebSocket relay, built once at boot.
fn ws_tls_config() -> Option<Arc<rustls::ClientConfig>> {
    let result = rustls_native_certs::load_native_certs();
    for error in &result.errors {
        tracing::warn!(error = %error, "error loading a native root certificate");
    }
    let mut roots = rustls::RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(result.certs);
    if ignored > 0 {
        tracing::warn!(
            added,
            ignored,
            "some native root certificates were unusable"
        );
    }
    if added == 0 {
        tracing::warn!("no usable native root certificates; wss upstreams cannot be reached");
        return None;
    }
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(Arc::new(config))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn plane(allow_http: bool) -> DataPlane {
        DataPlane::new(DpConfig {
            ttfb: Duration::from_secs(5),
            allow_http_upstream: allow_http,
            max_body_size: 1024,
            breaker: Thresholds::default(),
            ws_idle_timeout: Duration::from_secs(60),
        })
        .expect("data plane")
    }

    fn target(host: &str, port: u16, secure: bool) -> Target {
        Target {
            host: host.to_owned(),
            port,
            secure,
        }
    }

    #[tokio::test]
    async fn the_url_carries_the_scheme_and_the_authority() {
        let plane = plane(true);
        assert_eq!(
            plane.upstream_url(&target("api.example.com", 443, true), "/v1/x?q=1"),
            "https://api.example.com/v1/x?q=1"
        );
        assert_eq!(
            plane.upstream_url(&target("api.example.com", 8080, false), "v1/x"),
            "http://api.example.com:8080/v1/x"
        );
    }

    #[tokio::test]
    async fn plaintext_is_refused_only_when_the_flag_is_off() {
        assert!(plane(true).check_transport(&target("h", 80, false)).is_ok());
        let err = plane(false)
            .check_transport(&target("h", 80, false))
            .unwrap_err();
        assert_eq!(err.status_value(), 503, "{err}");
        assert!(
            plane(false)
                .check_transport(&target("h", 443, true))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_rate_limit_is_enforced_and_reported() {
        let plane = plane(true);
        let mut request = crate::infra::context::PluginRequest {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            target: target("h", 443, true),
            alias: "h".to_owned(),
            upstream_id: "u".to_owned(),
            route_id: Some("r".to_owned()),
            tenant_id: "t".to_owned(),
            subject: Some("s".to_owned()),
            request_id: "req".to_owned(),
            content_type: None,
            auth_config: std::collections::BTreeMap::new(),
            plugin_config: std::collections::BTreeMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
        };
        let limit = crate::domain::model::RateLimit {
            sharing: crate::domain::model::SharingMode::Inherit,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: crate::domain::model::SustainedRate {
                rate: 1,
                window: crate::domain::model::RateWindow::Minute,
            },
            burst: None,
            scope: crate::domain::model::RateScope::User,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        };
        let decision = plane
            .check_rate(Some(&limit), &request)
            .expect("first")
            .expect("a decision is reported when the limit is enforced");
        assert_eq!(decision.remaining, 0, "{decision:?}");
        let err = plane.check_rate(Some(&limit), &request).unwrap_err();
        assert_eq!(err.status_value(), 429, "{err}");
        assert_eq!(err.retry_after_secs(), Some(60));
        // ADR 0003: the decision reaches the client as both body extensions
        // and `X-RateLimit-*` headers.
        let headers = err.response_headers();
        for name in [
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ] {
            assert!(
                headers.iter().any(|(k, _)| k == name),
                "{name} missing from {headers:?}"
            );
        }
        assert_eq!(
            axum::response::IntoResponse::into_response(err)
                .headers()
                .get("x-ratelimit-limit"),
            Some(&"1".parse().expect("header value"))
        );
        // A different subject has its own bucket.
        request.subject = Some("other".to_owned());
        plane.check_rate(Some(&limit), &request).expect("other");
        // No policy at all is never a rejection, and reports nothing.
        assert!(
            plane
                .check_rate(None, &request)
                .expect("unbounded")
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_breaker_opens_after_enough_failures() {
        let plane = plane(true);
        let endpoint = target("down", 443, true);
        plane.check_breaker(&endpoint).expect("closed");
        for _ in 0..plane.breaker.thresholds().failure_threshold {
            plane.record(&endpoint, Some(&OagwError::link_unavailable("refused")));
        }
        let err = plane.check_breaker(&endpoint).unwrap_err();
        assert_eq!(err.status_value(), 503, "{err}");
        assert!(err.retry_after_secs().is_some(), "{err}");
        // A 404 says nothing about reachability.
        plane.breaker.clear();
        for _ in 0..plane.breaker.thresholds().failure_threshold {
            plane.record(&endpoint, Some(&OagwError::route_not_found("nope")));
        }
        plane.check_breaker(&endpoint).expect("still closed");
    }

    #[test]
    fn transport_errors_map_to_the_documented_statuses() {
        assert_eq!(
            map_transport(toolkit_http::HttpError::Timeout(Duration::from_secs(1))).status_value(),
            504
        );
        assert_eq!(
            map_transport(toolkit_http::HttpError::Tls("bad cert".into())).status_value(),
            503
        );
        assert_eq!(
            map_transport(toolkit_http::HttpError::ServiceClosed).status_value(),
            502
        );
    }
}
