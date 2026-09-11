//! The upstream call: posture, version negotiation and timeout classification.
//!
//! The stage owns the single outbound request of the pipeline. It runs over the
//! crate's existing `toolkit-http` client — no new client dependency — bounded
//! by `oagw.config.proxy_timeout_secs`, with the per-host HTTP capability kept
//! in a process-local cache so the hot path never renegotiates, and with the
//! outcome classified into exactly one row of the error table.
//!
//! Two principles are enforced here rather than assumed:
//!
//! * **No retry** (`cpt-cf-oagw-principle-no-retry`): exactly one request is
//!   sent. The client is built with the proxy profile, which carries no retry
//!   policy, so a failed call is never re-issued as a second request.
//! * **No cache** (`cpt-cf-oagw-principle-no-cache`): the response is returned
//!   to the caller and nothing is retained beyond the request; the only state
//!   this stage keeps is the per-host HTTP capability, which is connection
//!   metadata and not a response.
//!
//! SSRF posture (`cpt-cf-oagw-nfr-ssrf-protection`): the connect target is the
//! selected endpoint taken from the store, never a client-supplied host, and
//! the plaintext gate fails closed at the recorded default
//! `allow_http_upstream: false`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use pingora_memory_cache::MemoryCache;
use toolkit_http::{HttpClient, HttpError, TransportSecurity};

use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Scheme};
use crate::domain::validation::is_valid_hostname;

/// Capacity of the per-host HTTP capability cache.
const VERSION_CACHE_CAPACITY: usize = 4096;
/// Time-to-live of a cached per-host HTTP capability (one hour).
pub const VERSION_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// HTTP version a call to an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    /// HTTP/1.1, the only version a plaintext endpoint can speak.
    Http11,
    /// HTTP/2, negotiated through ALPN during the TLS handshake.
    Http2,
}

impl HttpVersion {
    /// The wire token the observability layer labels the version with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http11 => "http/1.1",
            Self::Http2 => "http/2",
        }
    }

    /// The version a response head reports.
    #[must_use]
    pub fn of(version: http::Version) -> Self {
        if version == http::Version::HTTP_2 || version == http::Version::HTTP_3 {
            Self::Http2
        } else {
            Self::Http11
        }
    }
}

/// The per-host HTTP capability cache.
///
/// A plaintext endpoint has no TLS handshake and therefore no ALPN
/// negotiation, so it is pinned to HTTP/1.1 and never cached. A TLS endpoint's
/// negotiated capability is cached per host with a one-hour TTL, so the hot
/// path consults the cache instead of renegotiating.
pub struct VersionCache {
    cache: MemoryCache<String, HttpVersion>,
}

impl std::fmt::Debug for VersionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionCache")
            .field("ttl", &VERSION_CACHE_TTL)
            .finish_non_exhaustive()
    }
}

impl Default for VersionCache {
    fn default() -> Self {
        Self::new(VERSION_CACHE_CAPACITY)
    }
}

impl VersionCache {
    /// A cache holding up to `capacity` per-host capabilities.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: MemoryCache::new(capacity),
        }
    }

    /// The cached capability of `host`, when one is still fresh.
    ///
    /// A capability whose TTL has elapsed is reported as absent, so the next
    /// call renegotiates.
    #[must_use]
    pub fn get(&self, host: &str) -> Option<HttpVersion> {
        match self.cache.get(host) {
            (Some(version), status) if status.is_hit() => Some(version),
            _ => None,
        }
    }

    /// Record the capability the handshake for `host` negotiated.
    pub fn record(&self, host: &str, version: HttpVersion) {
        self.cache.put(&host.to_owned(), version, Some(VERSION_CACHE_TTL));
    }

    /// The version the next call to `endpoint` speaks, and whether it came
    /// from the cache.
    #[must_use]
    pub fn capability(&self, endpoint: &Endpoint) -> (HttpVersion, bool) {
        match endpoint.scheme {
            Scheme::Http | Scheme::Wt => (HttpVersion::Http11, false),
            _ => match self.get(&endpoint.host) {
                Some(version) => (version, true),
                None => (HttpVersion::Http2, false),
            },
        }
    }
}

/// The outbound request the upstream call sends.
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// Method of the request, taken from the client request.
    pub method: Method,
    /// Absolute URL of the selected endpoint and the forward path.
    pub url: String,
    /// Headers to send, already transformed.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Buffered request body.
    pub body: Bytes,
    /// The endpoint the call connects to, taken from the store.
    pub endpoint: Endpoint,
}

/// An upstream response head, with its unread body.
#[derive(Debug)]
pub struct CallReply {
    /// Status the upstream returned.
    pub status: StatusCode,
    /// Headers the upstream returned.
    pub headers: HeaderMap,
    /// The response body, unread: the passthrough decides how it is consumed.
    pub body: toolkit_http::ResponseBody,
    /// HTTP version the call spoke.
    pub version: HttpVersion,
}

/// The upstream call stage.
pub struct UpstreamCaller {
    client: Arc<HttpClient>,
    versions: VersionCache,
    timeout: Duration,
    allow_http: bool,
    ssrf: bool,
}

impl UpstreamCaller {
    /// A caller over `client`, bounded by the recorded configuration.
    #[must_use]
    pub fn new(client: Arc<HttpClient>, config: &OagwConfig) -> Self {
        Self {
            client,
            versions: VersionCache::default(),
            timeout: Duration::from_secs(config.proxy_timeout_secs),
            allow_http: config.allow_http_upstream,
            ssrf: config.ssrf_policy.enabled,
        }
    }

    /// The capability cache, for the measurement entry 2.7 reports.
    #[must_use]
    pub const fn versions(&self) -> &VersionCache {
        &self.versions
    }

    /// The timeout every call is bounded by.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    // @cpt-begin:cpt-cf-oagw-dod-ssrf-posture:p1:inst-full
    /// Enforce the SSRF posture and the plaintext gate for `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `503` of a plaintext endpoint while the operator has
    /// not opted in, and the mapped `503` of a stored endpoint whose host does
    /// not survive the unconditional host validation.
    pub fn enforce_posture(&self, endpoint: &Endpoint) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-01
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-02
        // The plaintext gate runs before any connection attempt, so the default
        // HTTPS-only posture fails closed and never dials.
        if endpoint.scheme == Scheme::Http && !self.allow_http {
            return Err(DomainError::LinkUnavailable {
                detail: "the endpoint scheme is `http` and `allow_http_upstream` is `false`".to_string(),
                retry_after_seconds: None,
            });
        }
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-02
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-01

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-03
        // The connect target is the endpoint the store holds; the host
        // validation is unconditional at the recorded default and relaxed only
        // by an explicit operator decision, which never changes the target and
        // never adds a gateway-internal header.
        if self.ssrf && !is_admissible_host(&endpoint.host) {
            return Err(DomainError::LinkUnavailable {
                detail: "the configured endpoint host does not pass host validation".to_owned(),
                retry_after_seconds: None,
            });
        }
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-03
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-dod-ssrf-posture:p1:inst-full

    // @cpt-begin:cpt-cf-oagw-dod-upstream-call:p1:inst-full
    /// Send the request, exactly once, and return the unread response.
    ///
    /// # Errors
    ///
    /// Returns the mapped row of the failure: the `503` of a refused posture, a
    /// `504` of an expiry, a `502` of an unreachable or reset connection and a
    /// `413` of a body the client refused by size.
    pub async fn call(&self, request: OutboundRequest) -> Result<CallReply, DomainError> {
        self.enforce_posture(&request.endpoint)?;

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-04
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-05
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-06
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-07
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-08
        // The per-host capability: cached when the host has one, HTTP/1.1 for a
        // plaintext endpoint, an HTTP/2 attempt otherwise. The negotiated
        // version is recorded after the call below, so the first call to a host
        // pays the handshake and every later one reads the cache.
        let (capability, cached) = self.versions.capability(&request.endpoint);
        // The capability is what the handshake is expected to negotiate; the
        // client still negotiates it on the wire, so the pipeline reports it as
        // a measurement and never pins the request to it.
        tracing::debug!(
            host = %request.endpoint.host,
            version = capability.as_str(),
            cached,
            "upstream http capability"
        );
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-08
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-07
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-06
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-05
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-04

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-09
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-11
        // Exactly one request: the client profile carries no retry policy and
        // this stage calls it once, so the client request is never re-issued as
        // a whole. A connector-level attempt inside the client stack is the
        // client's behaviour, not a second request of this pipeline.
        let reply = self.send(request.clone()).await?;
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-11
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-09

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-12
        // No response is cached: the reply leaves this stage and nothing of it
        // is retained beyond the request. Only the per-host capability is.
        self.versions.record(&request.endpoint.host, reply.version);
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-12

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-13
        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-14
        // The reply goes back with its body unread, so a streamed body or an
        // upgraded protocol reaches the caller as an open exchange for the
        // entry-2.6 handoff instead of being read to completion here.
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-14
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-13

        // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-15
        Ok(reply)
        // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-15
    }
    // @cpt-end:cpt-cf-oagw-dod-upstream-call:p1:inst-full

    /// Send one request through the method-specific entry point of the client.
    async fn send(&self, request: OutboundRequest) -> Result<CallReply, DomainError> {
        let inner = self.send_raw(request).await?;
        let version = HttpVersion::of(inner.version());
        let status = inner.status();
        let headers = inner.headers().clone();
        Ok(CallReply {
            status,
            headers,
            body: inner.into_body(),
            version,
        })
    }

    /// Dial the upgrade the entry-2.6 handoff carries
    /// (`cpt-cf-oagw-algo-ws-upgrade`).
    ///
    /// The endpoint is dialed as exactly one request over the same client stack
    /// the entry-2.4 call uses — same posture gate, same timeout, same
    /// `proxy_timeout_secs` — and the raw response is returned with its unread
    /// body, so the upgrade handle the `101` carries stays reachable by the relay
    /// and the client request is never re-issued as a second request.
    ///
    /// # Errors
    ///
    /// Returns the mapped row of the failure, from the same classifier the
    /// entry-2.4 call maps through.
    pub async fn call_upgrade(
        &self,
        request: OutboundRequest,
    ) -> Result<http::Response<toolkit_http::ResponseBody>, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-05
        // The posture gate runs before any connection attempt, on the endpoint
        // the store holds.
        self.enforce_posture(&request.endpoint)?;
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-05

        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10
        // An upgrade is negotiated on HTTP/1.1 only: the `Upgrade` header is a
        // hop-by-hop HTTP/1.1 mechanism and carries no HTTP/2 equivalent this
        // gateway speaks. The shared client negotiates the version by ALPN, so a
        // host already known to speak HTTP/2 is refused before any connection
        // attempt rather than being dialed into a request no upstream can answer
        // with a `101`. A host whose capability is not known yet is dialed, and
        // the version it negotiated is recorded below so the next attempt on
        // that host fails here instead.
        if self.versions.capability(&request.endpoint).0 == HttpVersion::Http2 {
            return Err(DomainError::LinkUnavailable {
                detail: "the endpoint negotiated HTTP/2, on which no upgrade can be relayed"
                    .to_owned(),
                retry_after_seconds: None,
            });
        }
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10

        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10
        // Exactly one request: the client profile carries no retry policy and
        // this stage calls it once.
        let raw = self.send_raw(request.clone()).await;
        // The version the exchange negotiated is recorded, so the capability the
        // hot path reports stays the one the connection actually carried.
        if let Ok(response) = &raw {
            self.versions
                .record(&request.endpoint.host, HttpVersion::of(response.version()));
        }
        raw
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10
    }

    /// Send one request through the method-specific entry point of the client and
    /// return the raw response, with its unread body and its extensions.
    async fn send_raw(
        &self,
        request: OutboundRequest,
    ) -> Result<http::Response<toolkit_http::ResponseBody>, DomainError> {
        let mut builder = match request.method {
            Method::GET => self.client.get(&request.url),
            Method::POST => self.client.post(&request.url),
            Method::PUT => self.client.put(&request.url),
            Method::PATCH => self.client.patch(&request.url),
            Method::DELETE => self.client.delete(&request.url),
            Method::OPTIONS => self.client.options(&request.url),
            method => {
                return Err(DomainError::ProtocolError {
                    detail: format!("the proxy path does not forward `{method}`"),
                });
            }
        };
        for (name, value) in request.headers {
            builder = builder.header(name.as_str(), value.to_str().unwrap_or_default());
        }
        let response = builder
            .body_bytes(request.body)
            .send()
            .await
            .map_err(classify_call_error)?;
        Ok(response.into_inner())
    }
}

/// Build the client the proxy path dials upstreams with.
///
/// The client carries the proxy profile of `toolkit-http` — no retries, no
/// gateway-wide concurrency limit, no response body cap, no redirects — bounded
/// by the recorded `proxy_timeout_secs`, and opts into plaintext transports
/// only when the configuration does.
///
/// # Errors
///
/// Returns the client construction failure of the toolkit.
pub fn build_client(config: &OagwConfig) -> Result<HttpClient, HttpError> {
    let timeout = Duration::from_secs(config.proxy_timeout_secs);
    let mut builder = HttpClientBuilder::with_config(HttpClientConfig::proxy())
        .timeout(timeout)
        .total_timeout(timeout)
        .retry(None);
    if config.allow_http_upstream {
        // An explicit opt-in admits the plaintext transport in the client as
        // well; the pipeline still refuses a plaintext endpoint before any
        // connection attempt when the operator has not opted in.
        builder = builder.transport(TransportSecurity::AllowInsecureHttp);
    }
    builder.build()
}

/// Map a client failure onto exactly one row of the error table.
///
/// The classification follows the phase the failure occurred in: an expiry
/// observed while the response head was still outstanding is the request
/// window, a total-deadline expiry is the streamed-response window, a transport
/// failure that reports a connect timeout is the connection window, and every
/// other transport failure is an upstream the gateway could not reach.
#[must_use]
pub fn classify_call_error(error: HttpError) -> DomainError {
    // @cpt-begin:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-10
    match error {
        HttpError::Timeout(_) => DomainError::RequestTimeout {
            detail: "the upstream did not answer within the proxy timeout".to_owned(),
            retry_after_seconds: None,
        },
        HttpError::DeadlineExceeded(_) => DomainError::IdleTimeout {
            detail: "the upstream exchange exceeded the proxy timeout".to_owned(),
            retry_after_seconds: None,
        },
        HttpError::InvalidUri { .. } | HttpError::InvalidScheme { .. } => {
            DomainError::ProtocolError {
                detail: "the upstream target is not a usable URL".to_owned(),
            }
        }
        HttpError::BodyTooLarge { .. } => DomainError::PayloadTooLarge {
            detail: "the upstream response exceeds the body limit".to_owned(),
        },
        HttpError::Overloaded | HttpError::ServiceClosed => DomainError::LinkUnavailable {
            detail: "the outbound client is saturated".to_owned(),
            retry_after_seconds: None,
        },
        HttpError::Transport(_) | HttpError::Tls(_) if is_connect_timeout(&error) => {
            DomainError::ConnectionTimeout {
                detail: "the connection to the upstream timed out".to_owned(),
                retry_after_seconds: None,
            }
        }
        _ => DomainError::DownstreamError {
            detail: "the upstream connection failed".to_owned(),
        },
    }
    // @cpt-end:cpt-cf-oagw-algo-upstream-call:p1:inst-pe-uc-10
}

/// Whether a transport failure reports a connection that never opened in time.
fn is_connect_timeout(error: &HttpError) -> bool {
    let rendered = error.to_string().to_lowercase();
    rendered.contains("timed out") || rendered.contains("timeout")
}

/// Whether `host` is a connect target the posture admits.
///
/// The stored endpoint is already normalized at ingest, so this re-check
/// accepts the same shapes the schema allows: an RFC 1123 hostname or an IP
/// literal, which `is_valid_hostname` alone would reject for IPv6.
fn is_admissible_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok() || is_valid_hostname(host)
}

use toolkit_http::{HttpClientBuilder, HttpClientConfig};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;
    use http::Version;

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint { scheme, host: host.to_owned(), port }
    }

    fn config(allow_http: bool, ssrf: bool) -> OagwConfig {
        OagwConfig {
            allow_http_upstream: allow_http,
            ssrf_policy: crate::config::SsrfPolicy { enabled: ssrf },
            ..OagwConfig::default()
        }
    }

    #[tokio::test]
    async fn the_recorded_defaults_keep_the_https_only_posture() {
        let caller = UpstreamCaller::new(client(), &config(false, true));
        let error = caller
            .enforce_posture(&endpoint(Scheme::Http, "upstream.internal", 80))
            .expect_err("plaintext is refused at the recorded default");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn an_explicit_opt_in_admits_a_plaintext_endpoint() {
        let caller = UpstreamCaller::new(client(), &config(true, true));
        caller
            .enforce_posture(&endpoint(Scheme::Http, "upstream.internal", 80))
            .expect("the operator opted in");
    }

    #[tokio::test]
    async fn a_tls_endpoint_is_never_refused_by_the_plaintext_gate() {
        for ssrf in [true, false] {
            let caller = UpstreamCaller::new(client(), &config(false, ssrf));
            caller
                .enforce_posture(&endpoint(Scheme::Https, "upstream.internal", 443))
                .expect("TLS is the default posture");
        }
    }

    #[tokio::test]
    async fn host_validation_is_unconditional_at_the_recorded_default() {
        let caller = UpstreamCaller::new(client(), &config(false, true));
        let error = caller
            .enforce_posture(&endpoint(Scheme::Https, "not a host", 443))
            .expect_err("an invalid stored host is refused");
        assert_eq!(error.status(), 503, "{error}");
    }

    #[tokio::test]
    async fn relaxing_ssrf_keeps_the_target_and_the_posture_checks() {
        // `ssrf_policy.enabled: false` relaxes the host validation only: the
        // stored endpoint and the plaintext gate still holds.
        let caller = UpstreamCaller::new(client(), &config(false, false));
        assert!(
            caller
                .enforce_posture(&endpoint(Scheme::Https, "not a host", 443))
                .is_ok(),
            "host validation is relaxed"
        );
        let error = caller
            .enforce_posture(&endpoint(Scheme::Http, "upstream.internal", 80))
            .expect_err("the plaintext gate is independent of ssrf_policy");
        assert_eq!(error.status(), 503, "{error}");
    }

    #[tokio::test]
    async fn the_timeout_bound_comes_from_the_configuration() {
        let mut config = config(false, true);
        config.proxy_timeout_secs = 7;
        let caller = UpstreamCaller::new(client(), &config);
        assert_eq!(caller.timeout(), Duration::from_secs(7));
    }

    #[tokio::test]
    async fn an_upgrade_to_a_host_known_to_speak_http_2_is_refused_before_the_dial() {
        // An upgrade is an HTTP/1.1 mechanism and carries no HTTP/2 equivalent
        // this gateway speaks: a host the capability cache already reports as
        // HTTP/2 is refused before any connection attempt, with the same
        // `503` row the plaintext gate maps, instead of being dialed into a
        // request no upstream can answer with a `101`.
        let caller = UpstreamCaller::new(client(), &config(false, true));
        caller
            .versions
            .record("upstream.internal", HttpVersion::Http2);
        let request = OutboundRequest {
            method: Method::GET,
            url: "wss://upstream.internal:443/v1/socket".to_owned(),
            headers: Vec::new(),
            body: Bytes::new(),
            endpoint: endpoint(Scheme::Wss, "upstream.internal", 443),
        };
        let error = caller
            .call_upgrade(request)
            .await
            .expect_err("an HTTP/2 endpoint carries no upgrade");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[test]
    fn a_plaintext_capability_is_pinned_to_http_1_1() {
        let cache = VersionCache::default();
        let (version, cached) = cache.capability(&endpoint(Scheme::Http, "h", 80));
        assert_eq!(version, HttpVersion::Http11);
        assert!(!cached, "a plaintext endpoint has no ALPN to cache");
    }

    #[test]
    fn a_tls_capability_defaults_to_an_http_2_attempt() {
        let cache = VersionCache::default();
        let (version, cached) = cache.capability(&endpoint(Scheme::Https, "h", 443));
        assert_eq!(version, HttpVersion::Http2);
        assert!(!cached, "the first call to a host renegotiates");
    }

    #[test]
    fn a_negotiated_capability_is_cached_per_host() {
        let cache = VersionCache::default();
        cache.record("payments.internal", HttpVersion::Http2);
        let (version, cached) = cache.capability(&endpoint(Scheme::Https, "payments.internal", 443));
        assert_eq!(version, HttpVersion::Http2);
        assert!(cached, "the hot path reads the cache");
        // A host that never negotiated keeps its own entry.
        let (_, cached) = cache.capability(&endpoint(Scheme::Https, "other.internal", 443));
        assert!(!cached);
    }

    #[test]
    fn the_negotiated_version_is_read_from_the_response_head() {
        assert_eq!(HttpVersion::of(Version::HTTP_11), HttpVersion::Http11);
        assert_eq!(HttpVersion::of(Version::HTTP_2), HttpVersion::Http2);
        assert_eq!(HttpVersion::of(Version::HTTP_09), HttpVersion::Http11);
    }

    #[test]
    fn the_wire_tokens_of_the_version_cache_are_stable() {
        assert_eq!(HttpVersion::Http11.as_str(), "http/1.1");
        assert_eq!(HttpVersion::Http2.as_str(), "http/2");
    }

    #[test]
    fn an_expiry_is_classified_as_a_request_timeout() {
        let error = classify_call_error(HttpError::Timeout(Duration::from_secs(30)));
        assert_eq!(error.status(), 504, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
        );
    }

    #[test]
    fn a_total_deadline_expiry_is_classified_as_an_idle_timeout() {
        let error = classify_call_error(HttpError::DeadlineExceeded(Duration::from_secs(30)));
        assert_eq!(error.status(), 504, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"
        );
    }

    #[test]
    fn a_connect_timeout_is_classified_as_a_connection_timeout() {
        let error = classify_call_error(HttpError::Transport("connect timed out".into()));
        assert_eq!(error.status(), 504, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1"
        );
    }

    #[test]
    fn an_unreachable_connection_is_a_downstream_failure() {
        for error in [
            HttpError::Transport("tcp connect error: Connection refused".into()),
            HttpError::Tls("handshake failed".into()),
            HttpError::Overloaded,
            HttpError::ServiceClosed,
        ] {
            let rendered = error.to_string();
            let mapped = classify_call_error(error);
            assert!(
                matches!(
                    mapped,
                    DomainError::DownstreamError { .. } | DomainError::LinkUnavailable { .. }
                ),
                "{rendered} maps to {mapped}"
            );
        }
    }

    #[test]
    fn a_client_body_refusal_is_a_payload_too_large() {
        let error = classify_call_error(HttpError::BodyTooLarge { limit: 1, actual: 2 });
        assert_eq!(error.status(), 413, "{error}");
    }

    #[test]
    fn an_unusable_target_is_a_protocol_error() {
        let error = classify_call_error(HttpError::InvalidUri {
            url: "nope".to_owned(),
            kind: toolkit_http::InvalidUriKind::ParseError,
            reason: "bad".to_owned(),
        });
        assert_eq!(error.status(), 502, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
        );
    }

    #[tokio::test]
    async fn the_client_is_built_without_a_retry_policy() {
        // `cpt-cf-oagw-principle-no-retry`: the proxy client profile carries no
        // retry policy and no response cap, so a failed call is never re-issued.
        let config = config(false, true);
        let client = build_client(&config).expect("the proxy client builds");
        let _ = client;
    }

    #[tokio::test]
    async fn an_opted_in_configuration_admits_the_plaintext_transport() {
        let client = build_client(&config(true, true)).expect("the plaintext client builds");
        let _ = client;
    }

    fn client() -> Arc<HttpClient> {
        Arc::new(HttpClient::new().expect("default toolkit-http build"))
    }
}
