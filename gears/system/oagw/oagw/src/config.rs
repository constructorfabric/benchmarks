//! Gear-level configuration (`gears.oagw.config` in the server YAML).

use std::time::Duration;

use serde::Deserialize;

/// Hard ceiling on a buffered proxy request body (`cpt-cf-oagw-constraint-body-limit`).
pub const BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

/// Configuration for the OAGW gear.
///
/// Every field has a default so the gear starts with an empty `config:` block.
/// Unknown keys are tolerated: an operator config that carries settings for a
/// newer build must not stop this one from booting.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Wall-clock budget for connecting to an upstream and reading its
    /// response head. Streaming bodies (SSE, WebSocket) are not bounded by it.
    pub proxy_timeout_secs: u64,
    /// TCP/TLS connect budget for a single upstream endpoint.
    pub connect_timeout_secs: u64,
    /// Permit plaintext (`http`/`ws`) connections to upstreams.
    ///
    /// This governs whether a plaintext *connection is made*, not which
    /// schemes the management API accepts — `cpt-cf-oagw-constraint-https-only`
    /// is the default posture and this flag is what lifts it.
    pub allow_http_upstream: bool,
    /// Maximum buffered request body before `413 PayloadTooLarge`.
    pub max_request_body_bytes: usize,
    /// Ceiling for a cached OAuth2 access token (ADR 0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum number of cached OAuth2 access tokens (ADR 0008).
    pub token_cache_capacity: usize,
    /// Default `$top` for the management list endpoints.
    pub default_page_size: usize,
    /// Maximum accepted `$top` for the management list endpoints.
    pub max_page_size: usize,
    /// TTL for a cached tenant ancestor chain.
    pub tenant_cache_ttl_secs: u64,
    /// Age after which an unlinked custom plugin becomes collectable.
    pub plugin_gc_ttl_secs: u64,
    /// How often the plugin garbage collector sweeps.
    pub plugin_gc_tick_secs: u64,
    /// SSRF guardrails applied to every resolved upstream address.
    pub ssrf_policy: SsrfPolicy,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            connect_timeout_secs: 10,
            allow_http_upstream: false,
            max_request_body_bytes: BODY_LIMIT_BYTES,
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            default_page_size: 50,
            max_page_size: 100,
            tenant_cache_ttl_secs: 300,
            plugin_gc_ttl_secs: 30 * 24 * 60 * 60,
            plugin_gc_tick_secs: 3600,
            ssrf_policy: SsrfPolicy::default(),
        }
    }
}

impl OagwConfig {
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.max(1))
    }

    #[must_use]
    pub fn plugin_gc_tick(&self) -> Duration {
        Duration::from_secs(self.plugin_gc_tick_secs.max(1))
    }

    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs.max(1))
    }

    /// Clamp a requested page size into `[1, max_page_size]`.
    #[must_use]
    pub fn clamp_page_size(&self, requested: Option<usize>) -> usize {
        let max = self.max_page_size.max(1);
        requested.unwrap_or(self.default_page_size).clamp(1, max)
    }

    /// Validate internally inconsistent combinations.
    ///
    /// # Errors
    ///
    /// Returns a message describing the first inconsistency found.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_body_bytes == 0 {
            return Err("max_request_body_bytes must be greater than zero".to_owned());
        }
        if self.max_request_body_bytes > BODY_LIMIT_BYTES {
            return Err(format!(
                "max_request_body_bytes must not exceed the {BODY_LIMIT_BYTES} byte hard limit"
            ));
        }
        if self.max_page_size == 0 {
            return Err("max_page_size must be greater than zero".to_owned());
        }
        Ok(())
    }
}

/// Server-Side Request Forgery guardrails (`cpt-cf-oagw-nfr-ssrf-protection`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Master switch. When off no address checks are applied — intended for
    /// local development and E2E runs against loopback mock servers.
    pub enabled: bool,
    /// Permit loopback destinations (`127.0.0.0/8`, `::1`).
    pub allow_loopback: bool,
    /// Permit RFC 1918 / unique-local destinations.
    pub allow_private: bool,
    /// Permit link-local destinations (`169.254.0.0/16`, `fe80::/10`) — these
    /// cover the cloud metadata endpoints and stay blocked by default.
    pub allow_link_local: bool,
    /// Destination ports that are never dialled.
    pub blocked_ports: Vec<u16>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_loopback: false,
            allow_private: false,
            allow_link_local: false,
            blocked_ports: Vec::new(),
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
