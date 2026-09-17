// Created: 2026-09-04 by Constructor Tech
//! Gear-level configuration of the OAGW gear, read from the
//! `gears.oagw.config` block of the host configuration
//! (`ctx.config_or_default::<OagwConfig>()`).
//!
//! The e2e configuration declares:
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
//! These keys are the lowest layer of the configuration hierarchy of
//! `docs/PRD.md` §5.5: gear-level defaults, overridden by upstream- and then
//! route-level values (see `crate::domain::resolve_policy`).

use serde::Deserialize;

/// Default upstream proxy timeout when `proxy_timeout_secs` is absent.
///
/// `docs/PRD.md` fixes no value; 30s is a conservative default for external
/// API calls (the e2e configuration deliberately tightens it to 2s).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Hard body size limit (`cpt-cf-oagw-constraint-body-limit`: 100MB, rejected
/// before buffering).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 104_857_600;

/// Gear-level configuration (`gears.oagw.config`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Upstream request timeout in seconds; exceeding it yields
    /// `504 Timeout` (retry advice `Yes`).
    pub proxy_timeout_secs: u64,
    /// Whether a plaintext (`http`) upstream endpoint may actually be dialed.
    ///
    /// This is an **egress-time** gate enforced by the data plane only: it
    /// never restricts which endpoint schemes a create request may carry
    /// (`http` is a legal scheme value), and it is never consulted by
    /// scheme validation.
    pub allow_http_upstream: bool,
    /// Server-side request forgery guard for upstream resolution.
    pub ssrf_policy: SsrfPolicy,
    /// Maximum request body size accepted from a client before buffering.
    pub max_body_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl OagwConfig {
    /// Effective proxy timeout as a `std::time::Duration`.
    #[must_use]
    pub const fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }

    /// `true` when the SSRF guard is enabled for upstream resolution.
    #[must_use]
    pub const fn ssrf_enabled(&self) -> bool {
        self.ssrf_policy.enabled
    }
}

/// Server-side request forgery protection policy (SSRF).
///
/// `docs/PRD.md` (`cpt-cf-oagw-nfr-ssrf-protection`): OAGW must validate DNS
/// resolution results, enforce IP pinning rules and strip well-known internal
/// headers. The gear-level switch only decides whether those checks run at
/// egress time; endpoint *validation* is unconditional.
///
/// The guard is opt-in: `enabled` defaults to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Run the SSRF guard (IP pinning, private-segment rejection) during
    /// upstream resolution.
    pub enabled: bool,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
