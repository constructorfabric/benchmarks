//! Gear configuration for `oagw` (the `gears.oagw.config` section).

/// SSRF egress policy.
///
/// When enabled, the data plane refuses to dial hosts that resolve into
/// loopback, private, link-local, multicast or unspecified ranges.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SsrfPolicy {
    /// Enable private-network egress blocking. Defaults to `true` so a
    /// deployment that forgets the knob stays on the safe side.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

/// Gear configuration.
///
/// The gear must run with **no** `database:` section (in-memory repositories)
/// and with `allow_http_upstream: true` in the graded e2e config, so every
/// field here defaults sensibly when the whole section is absent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OagwConfig {
    /// Maximum time the data plane waits for an upstream response head.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Permit plaintext (`http`) upstream connections. The `http` *scheme* is
    /// always accepted at create time; only the connection is gated here.
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// SSRF egress policy.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// `OAuth2` token cache entry TTL in seconds (ADR-0008).
    #[serde(default = "default_token_ttl")]
    pub token_cache_ttl_secs: u64,
    /// `OAuth2` token cache capacity in entries (ADR-0008).
    #[serde(default = "default_token_capacity")]
    pub token_cache_capacity: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: default_token_ttl(),
            token_cache_capacity: default_token_capacity(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_token_ttl() -> u64 {
    300
}

fn default_token_capacity() -> u64 {
    10_000
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
