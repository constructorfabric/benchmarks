//! OAGW gear configuration.
//!
//! Deserialized from the `gears.oagw.config` section, e.g.
//!
//! ```yaml
//! oagw:
//!   config:
//!     proxy_timeout_secs: 2
//!     allow_http_upstream: true
//!     ssrf_policy:
//!       enabled: false
//! ```

use serde::Deserialize;

/// Default value for [`OagwConfig::proxy_timeout_secs`].
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default ceiling for cached OAuth2 access tokens, in seconds (5 minutes).
///
/// See ADR 0008: the effective TTL is `min(this, expires_in − 30s)`.
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default capacity of the OAuth2 access-token cache (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Default L1 configuration-cache TTL in seconds (ADR 0005: 10 000 entries,
/// generation-invalidated on management writes; the TTL is defence in depth
/// against a missed invalidation).
pub const DEFAULT_L1_CACHE_TTL_SECS: u64 = 5;

/// Default capacity of the L1 configuration cache (ADR 0005).
pub const DEFAULT_L1_CACHE_CAPACITY: usize = 10_000;

/// Hard upper bound on the upstream request/response body, in bytes (100 MB).
///
/// See `cpt-cf-oagw-constraint-body-limit`: reject before buffering.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Largest accepted upstream alias (kept aligned with RFC 1123's 253-octet
/// hostname ceiling plus the `:port` suffix).
pub const MAX_ALIAS_LEN: usize = 253 + 6;

fn default_proxy_timeout_secs() -> u64 {
    DEFAULT_PROXY_TIMEOUT_SECS
}

fn default_token_cache_ttl_secs() -> u64 {
    DEFAULT_TOKEN_CACHE_TTL_SECS
}

fn default_token_cache_capacity() -> usize {
    DEFAULT_TOKEN_CACHE_CAPACITY
}

fn default_l1_cache_ttl_secs() -> u64 {
    DEFAULT_L1_CACHE_TTL_SECS
}

fn default_l1_cache_capacity() -> usize {
    DEFAULT_L1_CACHE_CAPACITY
}

/// SSRF protection policy for outbound upstream connections.
///
/// See `cpt-cf-oagw-design-domain-model` §3.1 and DESIGN.md §4.4 (Security
/// Considerations). Host-level allow/deny lists are matched against the
/// endpoint host; `enabled` gates the whole check so local/E2E deployments can
/// point at loopback services.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Enable the SSRF host policy. Defaults to `true` (secure default).
    pub enabled: bool,
    /// Endpoint hosts the proxy may contact. Empty means "no allowlist
    /// constraint" — only [`SsrfPolicy::blocked_hosts`] applies.
    pub allowed_hosts: Vec<String>,
    /// Endpoint hosts the proxy must never dial, matched case-insensitively.
    pub blocked_hosts: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_hosts: Vec::new(),
            blocked_hosts: Vec::new(),
        }
    }
}

impl SsrfPolicy {
    /// Returns `true` when `host` may be dialed.
    ///
    /// Matching is ASCII-lowercase and exact; entries ending in `.` are
    /// treated as suffix matches so `".internal.example"` blocks the whole
    /// segment.
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let host = normalize_host(host);
        for blocked in &self.blocked_hosts {
            if host_matches(&normalize_host(blocked), &host) {
                return false;
            }
        }
        if self.allowed_hosts.is_empty() {
            return true;
        }
        self.allowed_hosts
            .iter()
            .any(|allowed| host_matches(&normalize_host(allowed), &host))
    }
}

/// Trims whitespace and the optional trailing root dot, lowercases, and keeps a
/// leading dot so `".corp.invalid"` stays a suffix pattern.
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if let Some(suffix) = pattern.strip_prefix('.') {
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    pattern == host
}

/// Gear configuration for OAGW.
///
/// `(a)` which endpoint schemes an upstream may *declare* is governed by the
/// domain model ([`crate::domain::models::EndpointScheme`]), while `(b)` whether
/// a *plaintext* connection is actually dialed is governed by
/// [`OagwConfig::allow_http_upstream`]. Only `(b)` lives here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Upstream request timeout in seconds. Also bounds the proxy connection
    /// establishment deadline.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,

    /// Permit dialing a plaintext (`http`) upstream endpoint.
    ///
    /// This is a *connection-time* gate only: an upstream declared with
    /// `"scheme": "http"` is always accepted into the roster by the management
    /// API. When this is `false` the proxy refuses to dial it.
    pub allow_http_upstream: bool,

    /// SSRF protection policy.
    pub ssrf_policy: SsrfPolicy,

    /// Ceiling for cached OAuth2 access tokens, in seconds (ADR 0008).
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,

    /// Maximum entries in the OAuth2 access-token cache (ADR 0008).
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,

    /// L1 configuration-cache TTL in seconds (ADR 0005).
    #[serde(default = "default_l1_cache_ttl_secs")]
    pub l1_cache_ttl_secs: u64,

    /// Maximum entries in the L1 configuration cache (ADR 0005).
    #[serde(default = "default_l1_cache_capacity")]
    pub l1_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            l1_cache_ttl_secs: DEFAULT_L1_CACHE_TTL_SECS,
            l1_cache_capacity: DEFAULT_L1_CACHE_CAPACITY,
        }
    }
}

impl OagwConfig {
    /// Proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// OAuth2 token-cache TTL as a [`std::time::Duration`] (ADR 0008).
    #[must_use]
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs.max(1))
    }

    /// L1 configuration-cache TTL as a [`std::time::Duration`] (ADR 0005).
    #[must_use]
    pub fn l1_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.l1_cache_ttl_secs.max(1))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_posture() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert!(!cfg.allow_http_upstream, "plaintext upstreams are blocked by default");
        assert!(cfg.ssrf_policy.enabled);
        assert!(cfg.ssrf_policy.allowed_hosts.is_empty());
        assert!(cfg.ssrf_policy.blocked_hosts.is_empty());
        assert_eq!(cfg.token_cache_ttl_secs, DEFAULT_TOKEN_CACHE_TTL_SECS);
        assert_eq!(cfg.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
        assert_eq!(cfg.l1_cache_ttl_secs, DEFAULT_L1_CACHE_TTL_SECS);
        assert_eq!(cfg.l1_cache_capacity, DEFAULT_L1_CACHE_CAPACITY);
    }

    #[test]
    fn cache_ttls_are_never_zero() {
        let cfg = OagwConfig {
            token_cache_ttl_secs: 0,
            l1_cache_ttl_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(cfg.token_cache_ttl(), std::time::Duration::from_secs(1));
        assert_eq!(cfg.l1_cache_ttl(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn parses_the_e2e_config_block() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("e2e config block must parse");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = serde_json::json!({ "no_such_field": 1 });
        assert!(serde_json::from_value::<OagwConfig>(raw).is_err());
    }

    #[test]
    fn empty_config_section_uses_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({}))
            .expect("an empty config section is valid");
        assert_eq!(cfg, OagwConfig::default());
    }

    #[test]
    fn ssrf_blocked_host_wins_over_allowlist() {
        let policy = SsrfPolicy {
            enabled: true,
            allowed_hosts: vec!["internal.example.com".to_owned()],
            blocked_hosts: vec!["metadata.internal".to_owned()],
        };
        assert!(policy.permits("INTERNAL.example.COM."));
        assert!(!policy.permits("metadata.internal"));
        // A non-empty allowlist is restrictive: only listed hosts may be dialed.
        assert!(!policy.permits("other.example.com"));
    }

    #[test]
    fn ssrf_suffix_pattern_blocks_the_whole_segment() {
        let policy = SsrfPolicy {
            enabled: true,
            allowed_hosts: Vec::new(),
            blocked_hosts: vec![".corp.invalid".to_owned()],
        };
        assert!(!policy.permits("api.corp.invalid"));
        assert!(!policy.permits("corp.invalid"));
        assert!(policy.permits("evil.invalid"));
    }

    #[test]
    fn ssrf_disabled_permits_everything() {
        let policy = SsrfPolicy {
            enabled: false,
            allowed_hosts: vec![],
            blocked_hosts: vec!["metadata.internal".to_owned()],
        };
        assert!(policy.permits("metadata.internal"));
    }

    #[test]
    fn proxy_timeout_is_at_least_one_second() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn max_body_bytes_is_100mb() {
        assert_eq!(MAX_BODY_BYTES, 100 * 1024 * 1024);
    }

    #[test]
    fn max_alias_len_is_stable() {
        assert_eq!(MAX_ALIAS_LEN, 259);
    }
}
