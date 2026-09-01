// Created: 2026-08-31 by Constructor Tech
//! Gear configuration (`gears.oagw.config`).
//!
//! Every field has a serde default and unknown keys are tolerated, so a
//! deployment may configure only the subset it cares about. The graded
//! E2E deployment configures:
//!
//! ```yaml
//! gears:
//!   oagw:
//!     config:
//!       proxy_timeout_secs: 2
//!       allow_http_upstream: true
//!       ssrf_policy:
//!         enabled: false
//! ```
//!
//! The data plane adds its own defaults for what the deployment leaves unset:
//! the circuit breaker of PRD `cpt-cf-oagw-nfr-high-availability` trips after 5
//! upstream health failures inside a 30s window and re-probes after 10s.

use std::time::Duration;

use serde::Deserialize;

/// Body size hard limit (DESIGN §2.2 `constraint-body-limit`): 100 MiB.
pub const MAX_BODY_BYTES_HARD_LIMIT: u64 = 100 * 1024 * 1024;

const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Ceiling of a cached `OAuth2` access token lifetime, in seconds (ADR-0008).
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Entries a cached `OAuth2` access token may occupy (ADR-0008).
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// How many head budgets a buffered body may spend in total.
const BODY_STREAM_BUDGET_FACTOR: u64 = 10;

/// Live WebSocket sessions the data plane bridges at once (PRD session flows).
const DEFAULT_MAX_WEBSOCKET_SESSIONS: usize = 1024;

/// Failures inside the window that trip the breaker (PRD threshold).
const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

/// Length of the sliding failure window, in seconds (PRD window).
const DEFAULT_FAILURE_WINDOW_SECS: u64 = 30;

/// How long an open breaker stays open before it admits one probe, in seconds.
///
/// Shorter than the default response-head budget (30s), so a client that
/// honours the `Retry-After` of a refusal is answered by a probe rather than by
/// another refusal; long enough that a breaker which re-opened is not hammered
/// again the moment it half-opens.
const DEFAULT_COOLDOWN_SECS: u64 = 10;

/// Outbound API Gateway configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct OagwConfig {
    /// Upstream request timeout in seconds (data plane).
    ///
    /// Budget of the **response head**: the time the dial plus the wait for the
    /// first response byte may take. The body phases have their own budgets
    /// ([`OagwConfig::proxy_idle_timeout_secs`] and
    /// [`OagwConfig::proxy_stream_timeout_secs`]) so a slow download cannot
    /// shorten them.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Silence tolerated between two frames of a forwarded body, in seconds.
    ///
    /// Unset means "the head budget". Applies to every response, an event
    /// stream included: a stream may pause, but not forever.
    #[serde(default)]
    pub proxy_idle_timeout_secs: Option<u64>,
    /// Overall budget of a non-event-stream body, in seconds.
    ///
    /// Unset means "a multiple of the head budget". An event stream has **no**
    /// overall budget (DESIGN §3.2 "Streaming"): it is bounded by silence only.
    #[serde(default)]
    pub proxy_stream_timeout_secs: Option<u64>,
    /// Whether plaintext (`http` / `ws`) upstream endpoints may be dialled.
    ///
    /// The endpoint `scheme` enum always accepts `http`; this switch only
    /// decides whether a plaintext connection is actually allowed.
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// Server-side request forgery guards.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// Hard request-body limit in bytes (DESIGN §2.2: 100 MiB).
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    /// How many WebSocket sessions may be live at once (PRD session flows).
    ///
    /// A session holds two sockets — the client's and the upstream's — for as
    /// long as the client keeps them, so without a ceiling a handful of clients
    /// could pin the whole data plane. The bound counts *bridged* sessions: the
    /// handshake itself is an ordinary request and spends the head budget, and
    /// the hand-over of the two sockets is bounded by that same budget, so a
    /// session that never opens cannot squat a slot.
    #[serde(default = "default_max_websocket_sessions")]
    pub max_websocket_sessions: usize,
    /// Ceiling of a cached `OAuth2` access token lifetime, in seconds
    /// (ADR-0008 "Gear-Level Configuration").
    ///
    /// Kept short because the cache has no invalidation mechanism yet: a
    /// rotated or revoked token stays served until it expires.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,
    /// Maximum entries of the `OAuth2` access-token cache (ADR-0008).
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
    /// Circuit breaker of the data plane (PRD `cpt-cf-oagw-nfr-high-availability`).
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            proxy_idle_timeout_secs: None,
            proxy_stream_timeout_secs: None,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_body_bytes: MAX_BODY_BYTES_HARD_LIMIT,
            max_websocket_sessions: DEFAULT_MAX_WEBSOCKET_SESSIONS,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

/// Bundle of the `OAuth2` access-token cache settings (ADR-0008).
///
/// Threaded through [`crate::infra::plugin::AuthPluginRegistry::with_builtins`]
/// into the two `OAuth2` client-credentials plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling of a cached token lifetime.
    pub ttl: Duration,
    /// Maximum number of cache entries.
    pub capacity: usize,
}

impl OagwConfig {
    /// Validation inputs derived from the configuration.
    ///
    /// Kept in one place so the write path cannot drift from the config keys.
    #[must_use]
    pub fn validation_policy(&self) -> crate::domain::validation::ValidationPolicy {
        crate::domain::validation::ValidationPolicy {
            allow_http_upstream: self.allow_http_upstream,
            max_body_bytes: self.max_body_bytes,
            ssrf: self.ssrf_policy.clone(),
        }
    }

    /// Cache settings of the `OAuth2` client-credentials plugins (ADR-0008).
    #[must_use]
    pub fn token_cache_config(&self) -> TokenCacheConfig {
        TokenCacheConfig {
            ttl: Duration::from_secs(self.token_cache_ttl_secs),
            capacity: self.token_cache_capacity,
        }
    }

    /// Budget of the dial plus the wait for the response head.
    #[must_use]
    pub fn head_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Silence tolerated between two frames of a forwarded body.
    #[must_use]
    pub fn body_idle_timeout(&self) -> Duration {
        Duration::from_secs(
            self.proxy_idle_timeout_secs
                .unwrap_or(self.proxy_timeout_secs),
        )
    }

    /// Overall budget of a forwarded body that is not an event stream.
    #[must_use]
    pub fn body_stream_timeout(&self) -> Duration {
        Duration::from_secs(
            self.proxy_stream_timeout_secs
                .unwrap_or(self.proxy_timeout_secs * BODY_STREAM_BUDGET_FACTOR),
        )
    }
}

/// Server-side request forgery policy (DESIGN §3.2 "Security Considerations").
#[derive(Debug, Clone, Deserialize)]
pub struct SsrfPolicy {
    /// Master switch. Enabled by default; the E2E deployment turns it off.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When non-empty, only these hosts may be dialled.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Hosts that may never be dialled.
    #[serde(default)]
    pub denied_hosts: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
        }
    }
}

/// Circuit breaker of the proxy data plane (PRD `cpt-cf-oagw-nfr-high-availability`).
///
/// "Circuit breakers MUST prevent cascade failures from unhealthy upstreams.
/// Threshold: 99.9% uptime; circuit breaker trips within 5 failed requests in
/// 30s window." The defaults are that threshold and that window.
///
/// The breaker decides whether a request may be dialled; it never answers for
/// the upstream. DESIGN §4.7 leaves "circuit breaker: config and fallback
/// strategies" to a later slice, so there is no fallback strategy here: an open
/// breaker answers `503 circuit_breaker.open.v1` with a `Retry-After`, and the
/// client retries (`Retriable: Yes`).
///
/// The block is `deny_unknown_fields`, the convention of the platform's other
/// config structs: a deployment that misspells `failure_threshold` wants a
/// refusal, not a breaker silently running on defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreakerConfig {
    /// Master switch. Enabled by default: the PRD names the breaker as a `p1`
    /// availability requirement, so a deployment has to turn it off rather
    /// than remember to turn it on.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Upstream health failures inside the window that trip the breaker.
    ///
    /// The PRD threshold is 5.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Length of the sliding window the failures are counted in, in seconds.
    ///
    /// The PRD window is 30s; failures older than it stop counting, so a
    /// healthy-again upstream does not stay one failure from a trip.
    #[serde(default = "default_failure_window_secs")]
    pub failure_window_secs: u64,
    /// How long an open breaker stays open before it admits one probe, in
    /// seconds.
    ///
    /// The default is 10s: shorter than the default response-head budget, so a
    /// client that honours the `Retry-After` is answered by a probe rather than
    /// by another refusal.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            failure_window_secs: DEFAULT_FAILURE_WINDOW_SECS,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        }
    }
}

const fn default_failure_threshold() -> u32 {
    DEFAULT_FAILURE_THRESHOLD
}

const fn default_failure_window_secs() -> u64 {
    DEFAULT_FAILURE_WINDOW_SECS
}

const fn default_cooldown_secs() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

const fn default_proxy_timeout_secs() -> u64 {
    DEFAULT_PROXY_TIMEOUT_SECS
}

const fn default_max_body_bytes() -> u64 {
    MAX_BODY_BYTES_HARD_LIMIT
}

const fn default_max_websocket_sessions() -> usize {
    DEFAULT_MAX_WEBSOCKET_SESSIONS
}

const fn default_token_cache_ttl_secs() -> u64 {
    DEFAULT_TOKEN_CACHE_TTL_SECS
}

const fn default_token_cache_capacity() -> usize {
    DEFAULT_TOKEN_CACHE_CAPACITY
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn defaults_match_the_documented_baseline() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.max_body_bytes, MAX_BODY_BYTES_HARD_LIMIT);
        assert_eq!(cfg.max_websocket_sessions, 1024);
        assert!(cfg.ssrf_policy.enabled);
        assert!(cfg.ssrf_policy.allowed_hosts.is_empty());
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.token_cache_config().ttl, Duration::from_mins(5));
        assert_eq!(cfg.token_cache_config().capacity, 10_000);
        assert_eq!(cfg.circuit_breaker, CircuitBreakerConfig::default());
    }

    /// The breaker's defaults are the PRD threshold and window, so a deployment
    /// that configures nothing still gets the availability the PRD asks for.
    #[test]
    fn the_breaker_defaults_are_the_prd_threshold_and_window() {
        let cfg = CircuitBreakerConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.failure_window_secs, 30);
        assert_eq!(cfg.cooldown_secs, 10);
    }

    #[test]
    fn the_breaker_is_configurable() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "circuit_breaker": {
                "enabled": false,
                "failure_threshold": 3,
                "failure_window_secs": 10,
                "cooldown_secs": 2
            }
        }))?;
        assert!(!cfg.circuit_breaker.enabled);
        assert_eq!(cfg.circuit_breaker.failure_threshold, 3);
        assert_eq!(cfg.circuit_breaker.failure_window_secs, 10);
        assert_eq!(cfg.circuit_breaker.cooldown_secs, 2);
        Ok(())
    }

    /// An absent breaker block is the PRD baseline, and one that names only some
    /// members keeps the defaults of the rest.
    #[test]
    fn a_partial_breaker_block_falls_back_to_the_defaults() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "circuit_breaker": { "failure_threshold": 2 }
        }))?;
        assert!(cfg.circuit_breaker.enabled);
        assert_eq!(cfg.circuit_breaker.failure_threshold, 2);
        assert_eq!(cfg.circuit_breaker.failure_window_secs, 30);
        assert_eq!(cfg.circuit_breaker.cooldown_secs, 10);
        Ok(())
    }

    /// The ceiling on live WebSocket sessions is a deployment knob: a tenant
    /// with many long-lived sessions raises it, a strict one lowers it.
    #[test]
    fn the_session_bound_is_configurable() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "max_websocket_sessions": 8
        }))?;
        assert_eq!(cfg.max_websocket_sessions, 8);
        Ok(())
    }

    #[test]
    fn the_token_cache_keys_are_configurable() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "token_cache_ttl_secs": 5,
            "token_cache_capacity": 7
        }))?;
        assert_eq!(cfg.token_cache_ttl_secs, 5);
        assert_eq!(cfg.token_cache_capacity, 7);
        Ok(())
    }

    /// The breaker block is the one place a typo is refused rather than
    /// silently ignored: `deny_unknown_fields` is the convention of the other
    /// gears' config structs (`toolkit-db`, `toolkit` telemetry and bootstrap),
    /// and a breaker that silently runs on defaults instead of the tuned values
    /// a deployment wrote is an availability regression nobody sees. The
    /// strictness is scoped to this struct, so a key unknown to the gear at the
    /// top level stays tolerated (`unknown_keys_are_tolerated`).
    #[test]
    fn a_misspelled_breaker_key_is_refused() {
        let err = serde_json::from_value::<OagwConfig>(serde_json::json!({
            "circuit_breaker": { "failure_treshold": 5 }
        }))
        .expect_err("a misspelled breaker key is not a breaker key");
        assert!(
            err.to_string().contains("failure_treshold"),
            "the error names the offending key: {err}"
        );

        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "circuit_breaker": { "failure_threshold": 5, "cooldown_secs": 7 }
        }))
        .expect("the documented keys still deserialize");
        assert_eq!(cfg.circuit_breaker.failure_threshold, 5);
        assert_eq!(cfg.circuit_breaker.cooldown_secs, 7);
    }

    #[test]
    fn deserialises_the_e2e_config_subtree() -> Result<(), Box<dyn Error>> {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw)?;
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        Ok(())
    }

    #[test]
    fn unknown_keys_are_tolerated() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({ "future_key": 1 }))?;
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        Ok(())
    }

    #[test]
    fn partial_subtree_falls_back_to_field_defaults() -> Result<(), Box<dyn Error>> {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({}))?;
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert_eq!(cfg.max_body_bytes, MAX_BODY_BYTES_HARD_LIMIT);
        assert_eq!(cfg.max_websocket_sessions, 1024);
        Ok(())
    }

    /// The three data-plane budgets are distinct phases: the head budget covers
    /// the dial and the wait for the first response byte, the idle budget the
    /// silence between two body frames and the stream budget the whole body.
    #[test]
    fn body_budgets_fall_back_to_the_head_budget() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2
        }))
        .unwrap_or_else(|error| panic!("the baseline config must parse: {error}"));
        assert_eq!(cfg.head_timeout(), std::time::Duration::from_secs(2));
        assert_eq!(cfg.body_idle_timeout(), std::time::Duration::from_secs(2));
        assert_eq!(
            cfg.body_stream_timeout(),
            std::time::Duration::from_secs(20)
        );

        let tuned: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "proxy_idle_timeout_secs": 15,
            "proxy_stream_timeout_secs": 600
        }))
        .unwrap_or_else(|error| panic!("the tuned config must parse: {error}"));
        assert_eq!(
            tuned.body_idle_timeout(),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            tuned.body_stream_timeout(),
            std::time::Duration::from_mins(10)
        );
    }

    #[test]
    fn ssrf_host_lists_are_kept() -> Result<(), Box<dyn Error>> {
        let raw = serde_json::json!({
            "ssrf_policy": { "enabled": false, "denied_hosts": ["metadata.internal"] }
        });
        let cfg: OagwConfig = serde_json::from_value(raw)?;
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.ssrf_policy.denied_hosts, vec!["metadata.internal"]);
        Ok(())
    }
}
