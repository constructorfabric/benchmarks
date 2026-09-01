//! Gear configuration for the OAGW outbound API gateway.
//!
//! Operator-facing knobs consumed by [`crate::gear::OagwGear::init`]. The
//! schema is flat (matching `gears.oagw.config` in `config/e2e-local.yaml`)
//! so a deployment only needs to override the keys it cares about.
//!
//! Every key except the ones the e2e config sets is defaulted: the e2e
//! config block is exactly
//! `{proxy_timeout_secs: 2, allow_http_upstream: true, ssrf_policy: {enabled: false}}`,
//! so all other fields MUST carry a `#[serde(default = "...")]` or be
//! reachable through a section-level `default`. Unknown keys are rejected
//! (`deny_unknown_fields`) so a typo surfaces as a loud `init` failure
//! instead of silently ignored configuration.

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// Gear configuration for `cf-gears-oagw`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OagwConfig {
    /// Data-plane proxy timeout in seconds. Applies to the upstream connect
    /// phase and to the overall request/response exchange. `0` is rejected by
    /// [`OagwConfig::validate`] (a zero timeout would abort every request).
    /// e2e value: `2`.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,

    /// Allow plaintext `http` (and `ws`) upstream endpoint schemes. `false`
    /// (the default) enforces the HTTPS-only MVP posture from the design
    /// constraints; the e2e config enables it so local mock upstreams can be
    /// reached without TLS.
    #[serde(default)]
    pub allow_http_upstream: bool,

    /// Server-side request forgery gate for resolved upstream hosts. Disabled
    /// by default; when enabled, slice 4 resolves and pins the target IP and
    /// rejects disallowed ranges before opening a connection.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,

    /// OAuth2 client-credentials token cache TTL in seconds (upper bound).
    /// See the OAuth2 ADR: the effective TTL is
    /// `min(self.token_cache_ttl_secs, expires_in - 30)`. `0` is rejected.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,

    /// Maximum number of cached upstream access tokens. `0` is rejected.
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,

    /// Control-plane L1 budget for the `upstream:{tenant_id}:{alias}` cache.
    /// `0` is rejected (the cache would be permanently empty).
    #[serde(default = "default_l1_entries")]
    pub upstream_l1_cache_max_entries: usize,

    /// Control-plane L1 budget for the `route:{upstream_id}:{method}:{path_prefix}` cache.
    /// `0` is rejected.
    #[serde(default = "default_l1_entries")]
    pub route_l1_cache_max_entries: usize,

    /// Control-plane L1 budget for the `plugin:{plugin_id}` cache. `0` is rejected.
    #[serde(default = "default_l1_entries")]
    pub plugin_l1_cache_max_entries: usize,

    /// Data-plane L1 budget for the resolved `(upstream, route)` hot cache.
    /// `0` is rejected.
    #[serde(default = "default_dp_entries")]
    pub dp_cache_max_entries: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            token_cache_capacity: default_token_cache_capacity(),
            upstream_l1_cache_max_entries: default_l1_entries(),
            route_l1_cache_max_entries: default_l1_entries(),
            plugin_l1_cache_max_entries: default_l1_entries(),
            dp_cache_max_entries: default_dp_entries(),
        }
    }
}

impl OagwConfig {
    /// Reject configurations that would produce undefined runtime behaviour:
    /// zero timeouts (every request would be aborted immediately) and zero
    /// cache capacities (every lookup would miss forever).
    ///
    /// # Errors
    ///
    /// Returns a message naming every invalid field, so a deployment with
    /// several misconfigured knobs sees all of them in one `init` failure.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut invalid: Vec<String> = Vec::new();

        if self.proxy_timeout_secs == 0 {
            invalid.push(
                "proxy_timeout_secs (must be > 0; zero would abort every request)".to_owned(),
            );
        }
        if self.token_cache_ttl_secs == 0 {
            invalid.push(
                "token_cache_ttl_secs (must be > 0; zero would expire every cached token instantly)"
                    .to_owned(),
            );
        }
        for (name, value) in [
            ("token_cache_capacity", self.token_cache_capacity),
            (
                "upstream_l1_cache_max_entries",
                self.upstream_l1_cache_max_entries,
            ),
            (
                "route_l1_cache_max_entries",
                self.route_l1_cache_max_entries,
            ),
            (
                "plugin_l1_cache_max_entries",
                self.plugin_l1_cache_max_entries,
            ),
            ("dp_cache_max_entries", self.dp_cache_max_entries),
        ] {
            if value == 0 {
                invalid.push(format!(
                    "{name} (must be > 0; zero makes the cache unusable)"
                ));
            }
        }
        invalid.extend(self.ssrf_policy.validate());

        if invalid.is_empty() {
            Ok(())
        } else {
            bail!("oagw configuration is invalid: {}", invalid.join("; "))
        }
    }

    /// Data-plane proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub const fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }
}

/// Server-side request forgery gate.
///
/// Disabled by default: the design requires HTTPS-only upstreams for the MVP
/// and leaves IP-pinning rules to a separate concern, so the gate only engages
/// when an operator explicitly turns it on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Master switch. `false` disables every check below.
    pub enabled: bool,

    /// Allow upstream hosts that resolve into private / link-local ranges.
    /// `true` (the default) keeps the on-premise deployment posture; set it to
    /// `false` together with `enabled` to block loopback, RFC 1918 and
    /// link-local targets.
    pub allow_private_networks: bool,

    /// Explicit CIDR allowlist (`a.b.c.d/nn`). Entries outside this list are
    /// rejected when the policy is enabled and the list is non-empty.
    pub allowed_ip_ranges: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_private_networks: true,
            allowed_ip_ranges: Vec::new(),
        }
    }
}

impl SsrfPolicy {
    /// Field-level checks shared with [`OagwConfig::validate`].
    fn validate(&self) -> Vec<String> {
        let mut invalid: Vec<String> = Vec::new();
        if !self.enabled {
            return invalid;
        }
        for range in &self.allowed_ip_ranges {
            if range.trim().is_empty() {
                invalid.push(
                    "ssrf_policy.allowed_ip_ranges (entries must be non-empty CIDR blocks)"
                        .to_owned(),
                );
            }
        }
        invalid
    }
}

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

fn default_token_cache_capacity() -> usize {
    10_000
}

fn default_l1_entries() -> usize {
    10_000
}

fn default_dp_entries() -> usize {
    1_000
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
