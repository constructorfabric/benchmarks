//! Gear configuration for the OAGW (outbound API gateway) module.
//!
//! The graded configuration (`config/e2e-local.yaml`) writes
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
//! so every field here is `#[serde(default)]` and the container **must not**
//! opt into `deny_unknown_fields`: operators may add keys this build does not
//! know about.

use serde::Deserialize;

/// Top-level `gears.oagw.config` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// End-to-end budget for a proxied request (connection + headers + body).
    pub proxy_timeout_secs: u64,
    /// Budget for a single connection attempt to an upstream.
    pub connect_timeout_secs: u64,
    /// Hard request-body ceiling for proxied requests (§3.2 body rules).
    pub max_request_body_bytes: usize,
    /// `http`-scheme upstreams are only legal when this is `true`.
    pub allow_http_upstream: bool,
    /// SSRF posture for outbound connections.
    pub ssrf_policy: SsrfPolicy,
    /// Capacity of the control-plane L1 cache (hot upstream/route configs).
    pub config_cache_capacity: u64,
    /// TTL of the control-plane L1 cache entries.
    pub config_cache_ttl_secs: u64,
    /// Capacity of the OAuth2 token cache (`pingora_memory_cache`).
    pub oauth2_token_cache_capacity: u64,
    /// `User-Agent` used when the client did not send one and passthrough is
    /// configured to forward it.
    pub default_upstream_user_agent: String,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            connect_timeout_secs: 10,
            max_request_body_bytes: 100 * 1024 * 1024,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            config_cache_capacity: 1024,
            config_cache_ttl_secs: 60,
            oauth2_token_cache_capacity: 4096,
            default_upstream_user_agent: String::from("cf-gears-oagw"),
        }
    }
}

/// Server-side request forgery posture applied before any outbound connect.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// When `false` (the e2e posture) no outbound address is filtered.
    pub enabled: bool,
    /// CIDR blocks that are always rejected when the policy is enabled.
    pub blocked_cidrs: Vec<String>,
    /// When non-empty and the policy is enabled, only these CIDR blocks may
    /// be dialed.
    pub allowed_cidrs: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            blocked_cidrs: vec![
                String::from("169.254.0.0/16"),
                String::from("127.0.0.0/8"),
                String::from("::1/128"),
            ],
            allowed_cidrs: Vec::new(),
        }
    }
}
