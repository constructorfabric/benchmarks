//! Gear configuration, loaded from `gears.oagw.config`.
//!
//! The gear has **no `database:` block** (`config/e2e-local.yaml`), so the
//! configuration carries no persistence settings: the control plane keeps its
//! state in memory (see `infra/storage`). Every field is defaulted so that a
//! gear entry with an empty `config` object (or no gear entry at all) still
//! yields a working configuration.

use serde::Deserialize;

/// Upper bound for proxied request bodies (DESIGN §2.2 `constraint-body-limit`).
pub const DEFAULT_MAX_PAYLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Default upstream proxy timeout in seconds (DESIGN §2.2 `constraint-body-limit`
/// companion: request must not hang forever).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default TTL for cached auth-plugin tokens in seconds.
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default capacity of the auth-plugin token cache.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Default time-to-live for unreferenced custom plugins (DESIGN §3.1
/// "Garbage Collection: ... after configurable TTL (default: 30 days)").
pub const DEFAULT_PLUGIN_GC_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Server-side request forgery policy for outbound connections.
///
/// Defaults to **disabled**: the MVP gateway relies on HTTPS-only upstream
/// transport (`constraint-https-only`) and on the target-host validation
/// performed by the routing layer. When enabled, the data plane additionally
/// refuses endpoints that fall outside the configured allow-list, which is
/// consumed by the proxy slices (S5+) — the control plane only persists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SsrfPolicyConfig {
    /// Whether SSRF hardening is applied on top of the HTTPS-only rule.
    pub enabled: bool,
    /// Allow-list of address segments (`IP`, `CIDR` or hostname suffix) an
    /// upstream endpoint must match. Empty means "no extra restriction".
    pub allowed_segments: Vec<String>,
    /// Refuse loopback, link-local and RFC 1918 ranges even when listed.
    pub deny_private_networks: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_segments: Vec::new(),
            deny_private_networks: true,
        }
    }
}

/// OAGW gear configuration (`gears.oagw.config`).
///
/// The top-level struct must stay lenient (`#[serde(default)]`, no
/// `deny_unknown_fields`): the platform merges gear configuration blocks
/// from several YAML sources and forwards unknown keys.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Total budget for the upstream call, in seconds.
    pub proxy_timeout_secs: u64,
    /// Allow `http` (plaintext) upstream endpoints. Security controls keep
    /// this `false` in production; `config/e2e-local.yaml` enables it for the
    /// shared e2e server only.
    pub allow_http_upstream: bool,
    /// TTL for auth-plugin token caches, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the auth-plugin token cache.
    pub token_cache_capacity: usize,
    /// SSRF hardening for outbound connections.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Hard limit on proxied request bodies, in bytes.
    pub max_payload_bytes: u64,
    /// How long an unreferenced custom plugin survives before GC, in seconds.
    pub plugin_gc_ttl_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            ssrf_policy: SsrfPolicyConfig::default(),
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            plugin_gc_ttl_secs: DEFAULT_PLUGIN_GC_TTL_SECS,
        }
    }
}

impl OagwConfig {
    /// Proxy timeout as a `std::time::Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Token-cache TTL as a `std::time::Duration`.
    #[must_use]
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs)
    }

    /// Plugin GC TTL as a `std::time::Duration`.
    #[must_use]
    pub fn plugin_gc_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.plugin_gc_ttl_secs)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
