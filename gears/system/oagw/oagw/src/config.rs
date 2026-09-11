// Updated: 2026-09-01 by Constructor Tech
//! Gear-level configuration for the OAGW gear.
//!
//! OAGW reads its own block from the host configuration file
//! (`oagw.config` in `config/e2e-local.yaml`). Everything here tunes the Data
//! Plane runtime; per-resource behaviour (upstreams, routes, plugins) lives in
//! the Control Plane store, not here.

use serde::Deserialize;
use std::time::Duration;

/// SSRF posture applied to upstream endpoints before a connection is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrfPolicy {
    /// When `false` no endpoint is screened (the development posture).
    pub enabled: bool,
    /// CIDR ranges never dialled when [`SsrfPolicy::enabled`].
    pub denied_cidrs: Vec<String>,
    /// Resolving an endpoint that yields no address is a hard failure.
    pub deny_unresolvable: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            denied_cidrs: vec![
                "127.0.0.0/8".to_owned(),
                "10.0.0.0/8".to_owned(),
                "172.16.0.0/12".to_owned(),
                "192.168.0.0/16".to_owned(),
                "169.254.0.0/16".to_owned(),
                "::1/128".to_owned(),
                "fc00::/7".to_owned(),
                "fe80::/10".to_owned(),
            ],
            deny_unresolvable: true,
        }
    }
}

/// Cache tuning for the OAuth2 client-credentials plugin (ADR-0008).
///
/// Threaded from the gear config into
/// [`crate::infra::plugin::registry::PluginRegistry::with_builtins`]; cloned
/// cheaply because every plugin variant carries its own copy.
#[derive(Debug, Clone)]
pub struct TokenCacheConfig {
    /// Ceiling applied to a cached token's TTL.
    pub ttl: Duration,
    /// Entries retained.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            capacity: 10_000,
        }
    }
}

impl From<&OagwConfig> for TokenCacheConfig {
    fn from(cfg: &OagwConfig) -> Self {
        Self {
            ttl: cfg.token_cache_ttl,
            capacity: cfg.token_cache_capacity,
        }
    }
}

/// Raw, `Deserialize`-shaped view of the gear config block.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OagwConfigRaw {
    pub proxy_timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    pub idle_timeout_secs: Option<u64>,
    pub allow_http_upstream: Option<bool>,
    pub max_payload_bytes: Option<usize>,
    pub token_cache_ttl_secs: Option<u64>,
    pub token_cache_capacity: Option<usize>,
    pub circuit_breaker: Option<CircuitBreakerRaw>,
    pub ssrf_policy: Option<SsrfPolicyRaw>,
    pub rate_limit: Option<RateLimitDefaultsRaw>,
    pub plugin_gc_ttl_secs: Option<u64>,
    /// Root path the management API and the proxy are mounted under.
    pub api_prefix: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerRaw {
    pub enabled: Option<bool>,
    pub failure_threshold: Option<u32>,
    pub window_secs: Option<u64>,
    pub open_duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SsrfPolicyRaw {
    pub enabled: Option<bool>,
    pub denied_cidrs: Option<Vec<String>>,
    pub deny_unresolvable: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RateLimitDefaultsRaw {
    pub sustained_rate: Option<f64>,
    pub window_secs: Option<u64>,
    pub burst_capacity: Option<f64>,
}

/// Resolved, validated OAGW gear configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct OagwConfig {
    /// Overall budget for one proxied request (connect + send + response).
    pub proxy_timeout: Duration,
    /// Time allowed for establishing the upstream connection.
    pub connect_timeout: Duration,
    /// Time an idle pooled connection is retained for.
    pub idle_timeout: Duration,
    /// Whether `scheme: "http"` upstreams are actually dialled in the clear.
    ///
    /// The *field* always accepts `http` — that is a schema question. This
    /// flag governs whether such a connection is ever made; when it is `false`
    /// the request is refused with a `ValidationError` before any socket is
    /// opened.
    pub allow_http_upstream: bool,
    /// Largest request body accepted, in bytes.
    pub max_payload_bytes: usize,
    /// TTL applied to cached OAuth2 tokens when the response omits `expires_in`.
    pub token_cache_ttl: Duration,
    /// Entries retained by the OAuth2 token cache.
    pub token_cache_capacity: usize,
    pub circuit_breaker: CircuitBreakerConfig,
    pub ssrf_policy: SsrfPolicy,
    pub rate_limit_defaults: RateLimitDefaults,
    /// How long a soft-deleted custom plugin is retained before collection.
    pub plugin_gc_ttl: Duration,
    /// Root path for the gear's API, including the gear name.
    pub api_prefix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    /// Consecutive failures inside the window that trip the breaker.
    pub failure_threshold: u32,
    pub window: Duration,
    pub open_duration: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitDefaults {
    /// Sustained permits per second when an upstream/route omits `rate_limit`.
    pub sustained_rate: f64,
    pub window: Duration,
    pub burst_capacity: f64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
            allow_http_upstream: false,
            max_payload_bytes: 100 * 1024 * 1024,
            token_cache_ttl: Duration::from_secs(300),
            token_cache_capacity: 10_000,
            circuit_breaker: CircuitBreakerConfig {
                enabled: true,
                failure_threshold: 5,
                window: Duration::from_secs(30),
                open_duration: Duration::from_secs(30),
            },
            ssrf_policy: SsrfPolicy::default(),
            rate_limit_defaults: RateLimitDefaults {
                sustained_rate: 100.0,
                window: Duration::from_secs(1),
                burst_capacity: 100.0,
            },
            plugin_gc_ttl: Duration::from_secs(30 * 24 * 60 * 60),
            api_prefix: "/oagw/v1".to_owned(),
        }
    }
}

impl OagwConfig {
    /// Build the resolved config from the raw, user-supplied block.
    #[must_use]
    pub fn from_raw(raw: &OagwConfigRaw) -> Self {
        let mut cfg = Self::default();

        if let Some(s) = raw.proxy_timeout_secs
            && s > 0
        {
            cfg.proxy_timeout = Duration::from_secs(s);
        }
        if let Some(s) = raw.connect_timeout_secs
            && s > 0
        {
            cfg.connect_timeout = Duration::from_secs(s);
        }
        cfg.idle_timeout = raw
            .idle_timeout_secs
            .map_or_else(|| cfg.idle_timeout, |s| Duration::from_secs(s.max(1)));
        cfg.allow_http_upstream = raw.allow_http_upstream.unwrap_or(false);
        if let Some(n) = raw.max_payload_bytes {
            cfg.max_payload_bytes = n;
        }
        if let Some(s) = raw.token_cache_ttl_secs {
            cfg.token_cache_ttl = Duration::from_secs(s.max(1));
        }
        if let Some(n) = raw.token_cache_capacity {
            cfg.token_cache_capacity = n.max(1);
        }
        if let Some(cb) = &raw.circuit_breaker {
            if let Some(v) = cb.enabled {
                cfg.circuit_breaker.enabled = v;
            }
            if let Some(v) = cb.failure_threshold {
                cfg.circuit_breaker.failure_threshold = v.max(1);
            }
            if let Some(v) = cb.window_secs {
                cfg.circuit_breaker.window = Duration::from_secs(v.max(1));
            }
            if let Some(v) = cb.open_duration_secs {
                cfg.circuit_breaker.open_duration = Duration::from_secs(v.max(1));
            }
        }
        if let Some(p) = &raw.ssrf_policy {
            if let Some(v) = p.enabled {
                cfg.ssrf_policy.enabled = v;
            }
            if let Some(v) = &p.denied_cidrs {
                cfg.ssrf_policy.denied_cidrs = v.clone();
            }
            if let Some(v) = p.deny_unresolvable {
                cfg.ssrf_policy.deny_unresolvable = v;
            }
        }
        if let Some(r) = &raw.rate_limit {
            if let Some(v) = r.sustained_rate {
                cfg.rate_limit_defaults.sustained_rate = v.max(0.0);
                cfg.rate_limit_defaults.burst_capacity = v.max(0.0);
            }
            if let Some(v) = r.window_secs {
                cfg.rate_limit_defaults.window = Duration::from_secs(v.max(1));
            }
            if let Some(v) = r.burst_capacity {
                cfg.rate_limit_defaults.burst_capacity = v.max(0.0);
            }
        }
        if let Some(s) = raw.plugin_gc_ttl_secs {
            cfg.plugin_gc_ttl = Duration::from_secs(s.max(1));
        }
        if let Some(p) = &raw.api_prefix {
            let trimmed = p.trim_matches('/');
            if !trimmed.is_empty() {
                cfg.api_prefix = format!("/{trimmed}");
            }
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout, Duration::from_secs(30));
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.max_payload_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.token_cache_ttl, Duration::from_secs(300));
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.circuit_breaker.failure_threshold, 5);
        assert_eq!(cfg.api_prefix, "/oagw/v1");
    }

    #[test]
    fn empty_raw_yields_defaults() {
        let raw = OagwConfigRaw::default();
        assert_eq!(OagwConfig::from_raw(&raw), OagwConfig::default());
    }

    #[test]
    fn e2e_block_is_honoured() {
        let raw: OagwConfigRaw = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .unwrap();
        let cfg = OagwConfig::from_raw(&raw);
        assert_eq!(cfg.proxy_timeout, Duration::from_secs(2));
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Untouched fields keep their defaults.
        assert_eq!(cfg.max_payload_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn zero_timeouts_fall_back_to_defaults() {
        let raw: OagwConfigRaw = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 0,
            "connect_timeout_secs": 0,
        }))
        .unwrap();
        let cfg = OagwConfig::from_raw(&raw);
        assert_eq!(cfg.proxy_timeout, Duration::from_secs(30));
        assert_eq!(cfg.connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn api_prefix_is_normalised() {
        let raw: OagwConfigRaw =
            serde_json::from_value(serde_json::json!({ "api_prefix": "oagw/api/v2/" })).unwrap();
        assert_eq!(OagwConfig::from_raw(&raw).api_prefix, "/oagw/api/v2");
        let raw: OagwConfigRaw =
            serde_json::from_value(serde_json::json!({ "api_prefix": "" })).unwrap();
        assert_eq!(OagwConfig::from_raw(&raw).api_prefix, "/oagw/v1");
    }
}
