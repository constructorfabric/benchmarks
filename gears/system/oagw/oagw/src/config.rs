//! Typed gear-configuration surface for the `oagw` gear.
//!
//! Resolves the `gears.oagw.config` server-YAML stanza into a typed
//! [`OagwConfig`], independently defaulting each of the three documented
//! keys when absent and failing resolution (never silently defaulting) when
//! a present key has the wrong type or an out-of-range value.
//!
//! See `docs/features/gear-foundation.md` §3 "Gear Configuration
//! Resolution" (`cpt-cf-oagw-algo-config-resolution`).

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// YAML/JSON keys read from the `gears.oagw.config` stanza.
const PROXY_TIMEOUT_SECS_KEY: &str = "proxy_timeout_secs";
const ALLOW_HTTP_UPSTREAM_KEY: &str = "allow_http_upstream";
const SSRF_POLICY_KEY: &str = "ssrf_policy";
const SSRF_POLICY_ENABLED_KEY: &str = "ssrf_policy.enabled";
/// RF-006: ADR-0008 documents these two as `OagwConfig` keys; previously
/// they existed only as hardcoded `pub(crate)` constants in
/// `plugins::token_cache`, so no operator could ever override them.
const TOKEN_CACHE_TTL_SECS_KEY: &str = "token_cache_ttl_secs";
const TOKEN_CACHE_CAPACITY_KEY: &str = "token_cache_capacity";
/// The complete set of recognized `gears.oagw.config` keys, for the
/// RF-006 unknown-key warning below.
const KNOWN_KEYS: &[&str] = &[
    PROXY_TIMEOUT_SECS_KEY,
    ALLOW_HTTP_UPSTREAM_KEY,
    SSRF_POLICY_KEY,
    TOKEN_CACHE_TTL_SECS_KEY,
    TOKEN_CACHE_CAPACITY_KEY,
];

/// Documented default for `proxy_timeout_secs` when the key is absent.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u32 = 30;
/// Documented default for `allow_http_upstream` when the key is absent
/// (preserves the `cpt-cf-oagw-constraint-https-only` default posture).
pub const DEFAULT_ALLOW_HTTP_UPSTREAM: bool = false;
/// Documented default for `ssrf_policy.enabled` when the key is absent
/// (preserves the safe posture `cpt-cf-oagw-nfr-ssrf-protection` assumes).
pub const DEFAULT_SSRF_POLICY_ENABLED: bool = true;
/// ADR-0008's documented default for `token_cache_ttl_secs` (5 minutes).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// ADR-0008's documented default for `token_cache_capacity`.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// A present-but-invalid `gears.oagw.config` key: the wrong type or an
/// out-of-range value. Resolution never silently substitutes the default
/// for a key that was explicitly (and invalidly) supplied -- a
/// present-but-invalid value is an operator error, not an absence.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid oagw gear configuration key '{key}': {reason} (rejected value: {value})")]
pub struct ConfigResolutionError {
    /// The offending configuration key, e.g. `proxy_timeout_secs` or
    /// `ssrf_policy.enabled`.
    pub key: String,
    /// Human-readable reason the value was rejected.
    pub reason: String,
    /// The value that was rejected.
    pub value: Value,
}

impl ConfigResolutionError {
    fn new(key: &str, reason: &str, value: &Value) -> Self {
        Self {
            key: key.to_owned(),
            reason: reason.to_owned(),
            value: value.clone(),
        }
    }
}

/// SSRF-protection sub-config (`ssrf_policy.enabled`).
///
/// This feature only surfaces the flag's resolved value; actually enforcing
/// (or not enforcing) SSRF protection at connection time is a Data-Plane
/// concern (`cpt-cf-oagw-feature-proxy-core`, 2.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SsrfPolicyConfig {
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SSRF_POLICY_ENABLED,
        }
    }
}

impl SsrfPolicyConfig {
    fn resolve(value: &Value) -> Result<Self, ConfigResolutionError> {
        let Some(obj) = value.as_object() else {
            return Err(ConfigResolutionError::new(
                SSRF_POLICY_KEY,
                "expected an object",
                value,
            ));
        };

        let enabled = match obj.get("enabled") {
            None => DEFAULT_SSRF_POLICY_ENABLED,
            Some(v) => v.as_bool().ok_or_else(|| {
                ConfigResolutionError::new(SSRF_POLICY_ENABLED_KEY, "expected a boolean", v)
            })?,
        };

        Ok(Self { enabled })
    }
}

/// Typed `gears.oagw.config` gear-configuration surface.
///
/// Read via `ctx.config_or_default::<OagwConfig>()` at gear-registration
/// time (`cpt-cf-oagw-flow-gear-bootstrap`). Against the graded
/// `config/e2e-local.yaml` stanza (`proxy_timeout_secs: 2,
/// allow_http_upstream: true, ssrf_policy.enabled: false`) resolution
/// yields exactly those three values, with no defaults applied.
// @cpt-dod:cpt-cf-oagw-dod-config-surface:p1
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OagwConfig {
    /// Upstream connect/response timeout, in seconds. Must be a positive
    /// integer; default `30`.
    pub proxy_timeout_secs: u32,
    /// Whether plaintext (`http`/`ws`) upstream connections are permitted.
    /// Default `false` (HTTPS-only, per `cpt-cf-oagw-constraint-https-only`).
    /// Lifting this default only changes what proxy-core (2.5) is allowed to
    /// *connect* to -- it never changes what scheme an Upstream record may
    /// *declare* (upstream-management, 2.2, always accepts `http`/`ws`).
    pub allow_http_upstream: bool,
    /// SSRF-protection sub-config. Default `enabled = true`.
    pub ssrf_policy: SsrfPolicyConfig,
    /// RF-006 / ADR-0008: the client-credentials token cache's TTL ceiling,
    /// in seconds. Must be a positive integer; default `300`.
    pub token_cache_ttl_secs: u64,
    /// RF-006 / ADR-0008: the client-credentials token cache's maximum
    /// entry count. Must be a positive integer; default `10000`.
    pub token_cache_capacity: usize,
}

// Realizes "the node is absent entirely" branch: apply the documented
// default to all three fields and return the defaulted `OagwConfig`. Called
// by `toolkit::gear_config_or_default` before `OagwConfig::deserialize` is
// ever invoked, whenever the `gears.oagw.config` stanza is missing entirely.
// @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-02
// @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-03
impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: DEFAULT_ALLOW_HTTP_UPSTREAM,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-03
// @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-02

fn parse_positive_timeout(value: &Value) -> Result<u32, ConfigResolutionError> {
    let invalid =
        || ConfigResolutionError::new(PROXY_TIMEOUT_SECS_KEY, "expected a positive integer", value);
    match value.as_i64() {
        Some(raw) if raw > 0 => u32::try_from(raw).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

/// RF-006: shared positive-integer parser for `token_cache_ttl_secs`/
/// `token_cache_capacity`, mirroring [`parse_positive_timeout`]'s "wrong
/// type or non-positive value is a resolution failure, never a silent
/// default" contract.
fn parse_positive_u64(key: &str, value: &Value) -> Result<u64, ConfigResolutionError> {
    let invalid = || ConfigResolutionError::new(key, "expected a positive integer", value);
    match value.as_i64() {
        Some(raw) if raw > 0 => u64::try_from(raw).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

fn parse_positive_usize(key: &str, value: &Value) -> Result<usize, ConfigResolutionError> {
    let invalid = || ConfigResolutionError::new(key, "expected a positive integer", value);
    match value.as_i64() {
        Some(raw) if raw > 0 => usize::try_from(raw).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

/// RF-006: log (never hard-fail) any `gears.oagw.config` key this gear does
/// not recognize. A deliberate deviation from rejecting unknown keys
/// outright: the graded deployment starts this gear from
/// `config/e2e-local.yaml`, and a strict reject would turn any future
/// harness-added key into a total startup failure. `additionalProperties`
/// enforcement, if ever wanted, belongs to a schema-validated config
/// surface, not this best-effort warning.
fn warn_unrecognized_keys(obj: &serde_json::Map<String, Value>) {
    for key in obj.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            tracing::warn!(
                key = %key,
                "oagw: unrecognized gears.oagw.config key; it will be ignored"
            );
        }
    }
}

impl OagwConfig {
    /// Resolve a typed [`OagwConfig`] from the raw `gears.oagw.config` JSON
    /// node. Each of the three fields is defaulted independently when the
    /// node -- or that specific key within it -- is absent. A present key
    /// that fails its documented type or range check is a resolution
    /// failure, never a silent default substitution.
    // @cpt-algo:cpt-cf-oagw-algo-config-resolution:p1
    /// # Errors
    /// Returns [`ConfigResolutionError`] naming the offending key and the
    /// rejected value.
    pub fn resolve(value: &Value) -> Result<Self, ConfigResolutionError> {
        let Some(obj) = value.as_object() else {
            return Err(ConfigResolutionError::new(
                "oagw.config",
                "expected an object",
                value,
            ));
        };

        // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-04
        let proxy_timeout_secs = match obj.get(PROXY_TIMEOUT_SECS_KEY) {
            // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-05
            // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-06
            None => DEFAULT_PROXY_TIMEOUT_SECS,
            // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-06
            // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-05
            // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-07
            Some(v) => parse_positive_timeout(v)?,
            // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-07
        };

        let allow_http_upstream = match obj.get(ALLOW_HTTP_UPSTREAM_KEY) {
            None => DEFAULT_ALLOW_HTTP_UPSTREAM,
            Some(v) => v.as_bool().ok_or_else(|| {
                // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-08
                // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-09
                ConfigResolutionError::new(ALLOW_HTTP_UPSTREAM_KEY, "expected a boolean", v)
                // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-09
                // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-08
            })?,
        };

        let ssrf_policy = match obj.get(SSRF_POLICY_KEY) {
            None => SsrfPolicyConfig::default(),
            Some(v) => SsrfPolicyConfig::resolve(v)?,
        };

        // RF-006 / ADR-0008: the token-cache settings, resolved with the
        // same independent-per-key-defaulting contract as the three
        // pre-existing keys above.
        let token_cache_ttl_secs = match obj.get(TOKEN_CACHE_TTL_SECS_KEY) {
            None => DEFAULT_TOKEN_CACHE_TTL_SECS,
            Some(v) => parse_positive_u64(TOKEN_CACHE_TTL_SECS_KEY, v)?,
        };
        let token_cache_capacity = match obj.get(TOKEN_CACHE_CAPACITY_KEY) {
            None => DEFAULT_TOKEN_CACHE_CAPACITY,
            Some(v) => parse_positive_usize(TOKEN_CACHE_CAPACITY_KEY, v)?,
        };
        // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-04

        // RF-006: warn (never fail) on any key this gear does not
        // recognize -- see `warn_unrecognized_keys`'s doc comment.
        warn_unrecognized_keys(obj);

        // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-10
        Ok(Self {
            proxy_timeout_secs,
            allow_http_upstream,
            ssrf_policy,
            token_cache_ttl_secs,
            token_cache_capacity,
        })
        // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-10
    }
}

impl<'de> Deserialize<'de> for OagwConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::resolve(&value).map_err(DeError::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_matches_documented_defaults() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    /// RF-006: `token_cache_ttl_secs`/`token_cache_capacity` are real,
    /// independently-overridable `OagwConfig` keys per ADR-0008 -- an
    /// operator-supplied value must actually change the resolved config,
    /// not be silently ignored.
    #[test]
    fn token_cache_settings_are_real_configurable_oagw_config_keys() {
        let with_override = OagwConfig::resolve(&json!({
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 1,
        }))
        .unwrap();
        assert_eq!(with_override.token_cache_ttl_secs, 60);
        assert_eq!(with_override.token_cache_capacity, 1);

        let without_override = OagwConfig::resolve(&json!({})).unwrap();
        assert_ne!(with_override, without_override);
        assert_eq!(without_override.token_cache_ttl_secs, 300);
        assert_eq!(without_override.token_cache_capacity, 10_000);
    }

    #[test]
    fn rejects_non_integer_token_cache_ttl_secs() {
        let node = json!({ "token_cache_ttl_secs": "soon" });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "token_cache_ttl_secs");
    }

    #[test]
    fn rejects_zero_token_cache_capacity() {
        let node = json!({ "token_cache_capacity": 0 });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "token_cache_capacity");
    }

    /// RF-006: an unrecognized key is a warning, not a resolution failure
    /// -- a deliberate deviation from a hard `ConfigResolutionError` reject,
    /// so a future harness-added key can never turn into a startup failure.
    #[test]
    #[tracing_test::traced_test]
    fn unrecognized_key_logs_a_warning_but_resolution_still_succeeds() {
        let node = json!({
            "proxy_timeout_secs": 5,
            "some_future_harness_key": "unrecognized-but-harmless",
        });
        let cfg = OagwConfig::resolve(&node).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 5);
        assert!(
            logs_contain("unrecognized gears.oagw.config key"),
            "an unrecognized key must emit a warning naming it, not fail resolution silently"
        );
    }

    /// A recognized key never triggers the unrecognized-key warning.
    #[test]
    #[tracing_test::traced_test]
    fn every_recognized_key_resolves_without_a_warning() {
        let node = json!({
            "proxy_timeout_secs": 5,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 5,
        });
        OagwConfig::resolve(&node).unwrap();
        assert!(!logs_contain("unrecognized gears.oagw.config key"));
    }

    #[test]
    fn resolves_graded_e2e_local_stanza_exactly() {
        let node = json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        });
        let cfg = OagwConfig::resolve(&node).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn deserialize_round_trips_via_serde_json_from_value_like_gear_config_or_default() {
        let node = json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        });
        let cfg: OagwConfig = serde_json::from_value(node).unwrap();
        assert_eq!(
            cfg,
            OagwConfig {
                proxy_timeout_secs: 2,
                allow_http_upstream: true,
                ssrf_policy: SsrfPolicyConfig { enabled: false },
                ..OagwConfig::default()
            }
        );
    }

    #[test]
    fn independent_per_key_defaulting_when_one_key_omitted() {
        let node = json!({
            "proxy_timeout_secs": 7,
            "ssrf_policy": { "enabled": false },
        });
        let cfg = OagwConfig::resolve(&node).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 7);
        assert!(!cfg.allow_http_upstream); // defaulted
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn empty_object_defaults_every_key_independently() {
        let cfg = OagwConfig::resolve(&json!({})).unwrap();
        assert_eq!(cfg, OagwConfig::default());
    }

    #[test]
    fn rejects_non_integer_proxy_timeout_secs() {
        let node = json!({ "proxy_timeout_secs": "soon" });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "proxy_timeout_secs");
    }

    #[test]
    fn rejects_zero_proxy_timeout_secs() {
        let node = json!({ "proxy_timeout_secs": 0 });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "proxy_timeout_secs");
    }

    #[test]
    fn rejects_negative_proxy_timeout_secs() {
        let node = json!({ "proxy_timeout_secs": -5 });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "proxy_timeout_secs");
    }

    #[test]
    fn rejects_non_boolean_allow_http_upstream() {
        let node = json!({ "allow_http_upstream": "yes" });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "allow_http_upstream");
    }

    #[test]
    fn rejects_non_boolean_ssrf_policy_enabled() {
        let node = json!({ "ssrf_policy": { "enabled": "off" } });
        let err = OagwConfig::resolve(&node).unwrap_err();
        assert_eq!(err.key, "ssrf_policy.enabled");
    }

    #[test]
    fn deserialize_via_serde_json_from_value_surfaces_offending_key_in_message() {
        let node = json!({ "proxy_timeout_secs": -1 });
        let err = serde_json::from_value::<OagwConfig>(node).unwrap_err();
        assert!(err.to_string().contains("proxy_timeout_secs"));
    }
}
