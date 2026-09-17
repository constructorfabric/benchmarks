//! Gear-level configuration for the OAGW gear.
//!
//! Populated from the `gears.oagw.config` section of the process
//! config (see `config/e2e-local.yaml`). All fields carry defaults so
//! the gear is bootable without an explicit section.

use serde::Deserialize;

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_connect_timeout_secs() -> u64 {
    5
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

fn default_token_cache_capacity() -> usize {
    10_000
}

fn default_false() -> bool {
    false
}

fn default_true() -> bool {
    true
}

/// OAGW gear configuration.
///
/// `Default` mirrors the serde fill-ins (see the `default_*` helpers) so
/// that constructing the struct directly — e.g. in tests — yields the
/// same bootable config as an absent YAML section (notably the wired
/// timeouts; a derived `Default` would hobble every proxied request
/// with 0ns connect/proxy budgets).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OagwConfig {
    /// Overall budget for a single proxied request (connect + send +
    /// receive). When the budget is exhausted the gateway responds 504
    /// `timeout.request`. Default: 30s.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Budget for establishing the upstream connection. Default: 5s.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Idle timeout between reads on a proxied connection. 0 disables.
    /// Default: 0 (no idle timeout).
    #[serde(default)]
    pub idle_timeout_secs: u64,
    /// Permit `http://` scheme on upstream endpoints. The upstream
    /// schema only admits `https`, `wss`, `wt`, `grpc`; plain HTTP is
    /// an explicit opt-in (dev / on-box proxies only).
    #[serde(default = "default_false")]
    pub allow_http_upstream: bool,
    /// Server-Side Request Forgery protections applied before
    /// forwarding (host allow-listing; see [`SsrfPolicy`]).
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for cached OAuth2 access tokens (seconds). The actual
    /// TTL is `min(config_ttl, expires_in - 30s)`. Default: 300.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache. Default: 10_000.
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
}

/// SSRF protection policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SsrfPolicy {
    /// Master switch. When `true` (default), upstream hosts must be
    /// reachable and, when `allowed_hosts` is non-empty, must belong
    /// to the allow-list (or be an IP literal inside an allowed
    /// subnet). The graded config disables it (`{enabled: false}`) so
    /// unlisted loopback upstreams remain routable.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicitly permitted upstream hosts (hostnames or IPs). Empty
    /// means "no host allow-list" (all hosts pass the policy gate).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// CIDR allow-list for IP-literal and hostname-resolved upstreams.
    /// Empty means "no CIDR restrictions".
    #[serde(default)]
    pub allowed_cidrs: Vec<String>,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            connect_timeout_secs: default_connect_timeout_secs(),
            idle_timeout_secs: 0,
            allow_http_upstream: default_false(),
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            token_cache_capacity: default_token_cache_capacity(),
        }
    }
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            allowed_hosts: Vec::new(),
            allowed_cidrs: Vec::new(),
        }
    }
}

impl OagwConfig {
    /// Overall request budget as a [`std::time::Duration`].
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Connection-establishment budget as a [`std::time::Duration`].
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_secs)
    }
}
