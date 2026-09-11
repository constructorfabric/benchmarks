//! OAGW gear configuration (`cpt-cf-oagw-dod-config-resolution`).
//!
//! [`OagwConfig`] is resolved exactly once at startup, in
//! [`crate::gear::OagwGear::init`], via `GearCtx::config_or_default`, and the
//! resolved snapshot is held for the life of the process instead of being
//! re-parsed per request.

use serde::Deserialize;

/// SSRF-guard toggle, nested under `ssrf_policy` in the gear configuration
/// section.
///
/// Defaults to `enabled: true` (the guard is on by default); the graded
/// deployment (`config/e2e-local.yaml`) explicitly disables it for local
/// testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicy {
    /// Whether the SSRF guard is active for outbound proxy requests.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Resolved OAGW gear configuration section
/// (`cpt-cf-oagw-dod-config-resolution`).
///
/// Matches the `oagw.config` block exactly, including `deny_unknown_fields`
/// so a typo in the configuration file fails gear startup rather than being
/// silently ignored.
///
/// ```yaml
/// oagw:
///   config:
///     proxy_timeout_secs: 2
///     allow_http_upstream: true
///     ssrf_policy:
///       enabled: false
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Upstream proxy request timeout, in seconds. Default `30`.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext (`http`/`ws`) upstream connections are permitted.
    /// Default `false`; the default posture requires `https`/`wss`/`wt`/`grpc`
    /// (`cpt-cf-oagw-constraint-https-only`).
    pub allow_http_upstream: bool,
    /// SSRF-guard toggle for outbound proxy requests.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling (in seconds) for a cached `oauth2_client_cred`/
    /// `oauth2_client_cred_basic` access token's lifetime
    /// (`cpt-cf-oagw-dod-token-cache`). The actual cache lifetime is the
    /// lower of this value and the token's own reported expiry minus the
    /// 30-second safety margin. Default `300` (5 minutes), matching
    /// ADR-0008. `#[serde(default)]` on the struct (below) supplies this
    /// when the graded `config/e2e-local.yaml` block omits it.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the `OAuth2` client-credentials token
    /// cache (`cpt-cf-oagw-dod-token-cache`). Default `10000`, matching
    /// ADR-0008.
    pub token_cache_capacity: usize,
}

const fn default_token_cache_ttl_secs() -> u64 {
    300
}

const fn default_token_cache_capacity() -> usize {
    10_000
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            token_cache_capacity: default_token_cache_capacity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OagwConfig, SsrfPolicy};
    use serde_json::json;

    // @cpt-begin:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-parse-test-01
    #[test]
    fn deserializes_the_graded_configuration_block_exactly() {
        // Mirrors `config/e2e-local.yaml`'s `oagw.config` block verbatim
        // (JSON is used here; it is a structural superset of the YAML block).
        let value = json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });

        let cfg: OagwConfig =
            serde_json::from_value(value).expect("graded config block must deserialize");

        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }
    // @cpt-end:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-parse-test-01

    #[test]
    fn defaults_apply_when_the_section_is_absent() {
        let cfg: OagwConfig =
            serde_json::from_value(json!({})).expect("an empty section must resolve to defaults");

        assert_eq!(cfg, OagwConfig::default());
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
    }

    #[test]
    fn defaults_apply_per_field_when_partially_specified() {
        let cfg: OagwConfig = serde_json::from_value(json!({ "proxy_timeout_secs": 7 }))
            .expect("a partial section must fill in defaults for absent fields");

        assert_eq!(cfg.proxy_timeout_secs, 7);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
    }

    // @cpt-begin:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-catch-test-01
    #[test]
    fn rejects_an_unknown_top_level_key() {
        let value = json!({ "proxy_timeout_secs": 5, "unexpected_key": true });
        let result: Result<OagwConfig, _> = serde_json::from_value(value);
        assert!(result.is_err(), "an unknown top-level key must be rejected");
    }

    #[test]
    fn rejects_an_unknown_ssrf_policy_key() {
        let value = json!({ "ssrf_policy": { "enabled": true, "unexpected_key": 1 } });
        let result: Result<OagwConfig, _> = serde_json::from_value(value);
        assert!(result.is_err(), "an unknown nested key must be rejected");
    }

    #[test]
    fn rejects_an_out_of_bounds_type_for_proxy_timeout_secs() {
        let value = json!({ "proxy_timeout_secs": "not-a-number" });
        let result: Result<OagwConfig, _> = serde_json::from_value(value);
        assert!(result.is_err(), "an invalid type must be rejected");
    }

    #[test]
    fn rejects_an_invalid_type_for_allow_http_upstream() {
        let value = json!({ "allow_http_upstream": "yes" });
        let result: Result<OagwConfig, _> = serde_json::from_value(value);
        assert!(result.is_err(), "an invalid type must be rejected");
    }

    /// Distinct from the "wrong type" cases above: `-1` is the right JSON
    /// *type* (a number) but an out-of-bounds *value* for an unsigned field,
    /// covering the "or out-of-bounds value" half of
    /// `cpt-cf-oagw-dod-config-resolution`'s startup-failure criterion.
    #[test]
    fn rejects_an_out_of_bounds_negative_value_for_proxy_timeout_secs() {
        let value = json!({ "proxy_timeout_secs": -1 });
        let result: Result<OagwConfig, _> = serde_json::from_value(value);
        assert!(
            result.is_err(),
            "a negative value must be rejected for an unsigned field"
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-catch-test-01

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-config-token-cache-defaults-test-01
    #[test]
    fn token_cache_knobs_default_when_absent() {
        let cfg: OagwConfig =
            serde_json::from_value(json!({})).expect("an empty section must resolve to defaults");
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn token_cache_knobs_are_read_when_present() {
        let value = json!({ "token_cache_ttl_secs": 60, "token_cache_capacity": 500 });
        let cfg: OagwConfig = serde_json::from_value(value).expect("must deserialize");
        assert_eq!(cfg.token_cache_ttl_secs, 60);
        assert_eq!(cfg.token_cache_capacity, 500);
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-config-token-cache-defaults-test-01

    #[test]
    fn ssrf_policy_deserializes_independently() {
        let cfg: SsrfPolicy = serde_json::from_value(json!({ "enabled": false }))
            .expect("ssrf policy block must deserialize");
        assert!(!cfg.enabled);
    }
}
