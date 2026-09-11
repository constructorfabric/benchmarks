//! `OagwConfig` — the gear's closed configuration surface.
//!
//! The key set is closed (wire contract 3): the six keys below, their defaults
//! and their validation rules are the whole surface. An unknown key, a
//! non-positive timeout, a zero body limit or an unaccepted upstream protocol
//! is a hard [`OagwError::ValidationError`] whose `detail` names the offending
//! key, so a misconfiguration surfaces at startup instead of being silently
//! re-defaulted.

// @cpt-begin:cpt-cf-oagw-dod-config-surface:p1:inst-full
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::error::OagwError;

/// Key of the upstream proxy timeout, in seconds.
pub const KEY_PROXY_TIMEOUT_SECS: &str = "proxy_timeout_secs";
/// Key of the plaintext-upstream switch (DECOMPOSITION correction 2).
pub const KEY_ALLOW_HTTP_UPSTREAM: &str = "allow_http_upstream";
/// Key of the SSRF-policy object.
pub const KEY_SSRF_POLICY: &str = "ssrf_policy";
/// Key of the buffered request-body hard limit, in bytes.
pub const KEY_BODY_LIMIT_BYTES: &str = "body_limit_bytes";
/// Key of the token-cache entry ceiling, in seconds.
pub const KEY_TOKEN_CACHE_TTL_SECS: &str = "token_cache_ttl_secs";
/// Key of the token-cache entry capacity.
pub const KEY_TOKEN_CACHE_CAPACITY: &str = "token_cache_capacity";

/// The closed set of `OagwConfig` keys, in declaration order.
pub const KNOWN_CONFIG_KEYS: &[&str] = &[
    KEY_PROXY_TIMEOUT_SECS,
    KEY_ALLOW_HTTP_UPSTREAM,
    KEY_SSRF_POLICY,
    KEY_BODY_LIMIT_BYTES,
    KEY_TOKEN_CACHE_TTL_SECS,
    KEY_TOKEN_CACHE_CAPACITY,
];

/// Key of the `enabled` flag inside the `ssrf_policy` object.
pub const KEY_SSRF_ENABLED: &str = "enabled";

/// Default `proxy_timeout_secs` (DESIGN/ADR default).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default `allow_http_upstream` (DECOMPOSITION correction 2).
pub const DEFAULT_ALLOW_HTTP_UPSTREAM: bool = false;
/// Default `ssrf_policy.enabled` (DESIGN/ADR default).
pub const DEFAULT_SSRF_ENABLED: bool = true;
/// Default body limit: 100 MB (`cpt-cf-oagw-constraint-body-limit`).
pub const DEFAULT_BODY_LIMIT_BYTES: u64 = 100 * 1024 * 1024;
/// Default `token_cache_ttl_secs` (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default `token_cache_capacity` (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Upstream protocols the gateway accepts (`cpt-cf-oagw-constraint-protocols`).
///
/// `http` is a legal protocol value: plaintext establishment is governed
/// separately by [`OagwConfig::allow_http_upstream`] (wire contract 2).
pub const UPSTREAM_PROTOCOLS: &[&str] = &["http", "https", "wss", "grpc", "wt"];

/// An accepted upstream protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamProtocol {
    /// Plaintext HTTP (allowed only when `allow_http_upstream` is `true`).
    Http,
    /// HTTPS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// gRPC.
    Grpc,
    /// WebTransport.
    Wt,
}

impl UpstreamProtocol {
    /// Parses an accepted upstream protocol value.
    ///
    /// The error is the typed configuration error naming the rejected value
    /// (`inst-cl-06`).
    pub fn parse(value: &str) -> Result<Self, OagwError> {
        match value {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "wss" => Ok(Self::Wss),
            "grpc" => Ok(Self::Grpc),
            "wt" => Ok(Self::Wt),
            other => Err(OagwError::validation_error(format!(
                "oagw.config: 'protocol' value '{other}' is not one of {}",
                UPSTREAM_PROTOCOLS.join(", ")
            ))),
        }
    }

    /// The canonical wire name of the protocol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Grpc => "grpc",
            Self::Wt => "wt",
        }
    }
}

/// SSRF-policy switches of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Whether SSRF protection is enabled.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SSRF_ENABLED,
        }
    }
}

/// The gear configuration, defaulted then overridden then validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Upstream request timeout, seconds; must be strictly positive.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext `http` upstreams may be established.
    pub allow_http_upstream: bool,
    /// SSRF-policy switches.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Buffered request-body hard limit, bytes; must be strictly positive.
    pub body_limit_bytes: u64,
    /// Token-cache entry ceiling, seconds; must be strictly positive.
    pub token_cache_ttl_secs: u64,
    /// Token-cache entry capacity.
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-02
        // Every default carries its concrete value: `proxy_timeout_secs` 30,
        // `allow_http_upstream` false (DECOMPOSITION correction 2),
        // `ssrf_policy.enabled` true, body limit 100 MB,
        // `token_cache_ttl_secs` 300 and `token_cache_capacity` 10000.
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: DEFAULT_ALLOW_HTTP_UPSTREAM,
            ssrf_policy: SsrfPolicyConfig::default(),
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-02
    }
}

impl OagwConfig {
    /// Loads the gear configuration from the raw `oagw.config` block.
    ///
    /// An absent block yields the defaults (`inst-gb-02`, `inst-gb-03`); a
    /// present block overrides the defaults key by key before validation runs
    /// (`inst-gb-04`), so an explicitly supplied value is never silently
    /// re-defaulted.
    ///
    /// # Errors
    /// Returns the typed configuration error naming the offending key when the
    /// block carries an unknown key, a value of the wrong type or an
    /// out-of-range value.
    pub fn load(raw: Option<&Value>) -> Result<Self, OagwError> {
        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-01
        // Parse the known `OagwConfig` keys out of the raw gear config value.
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-04
        // A present block overrides the defaults key by key, before
        // validation runs.
        let mut config = Self::default();
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-02
        let Some(raw) = raw else {
            // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-03
            // The block is absent: continue startup with the defaulted
            // OagwConfig.
            return Ok(config);
            // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-03
        };
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-02

        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-03
        // Apply the overrides: any key present in the raw `oagw.config` value
        // replaces the corresponding default before validation runs, so an
        // explicitly supplied value is never silently re-defaulted.
        let entries = raw.as_object().ok_or_else(|| {
            OagwError::validation_error(
                "oagw.config: the configuration block must be a mapping of known keys",
            )
        })?;
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-03

        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-04
        // FOR EACH key present in the raw config value.
        for (key, value) in entries {
            match key.as_str() {
                KEY_PROXY_TIMEOUT_SECS => {
                    config.proxy_timeout_secs = parse_u64(KEY_PROXY_TIMEOUT_SECS, value)?;
                }
                KEY_ALLOW_HTTP_UPSTREAM => {
                    config.allow_http_upstream = parse_bool(KEY_ALLOW_HTTP_UPSTREAM, value)?;
                }
                KEY_SSRF_POLICY => {
                    config.ssrf_policy = parse_ssrf_policy(KEY_SSRF_POLICY, value)?;
                }
                KEY_BODY_LIMIT_BYTES => {
                    config.body_limit_bytes = parse_u64(KEY_BODY_LIMIT_BYTES, value)?;
                }
                KEY_TOKEN_CACHE_TTL_SECS => {
                    config.token_cache_ttl_secs = parse_u64(KEY_TOKEN_CACHE_TTL_SECS, value)?;
                }
                KEY_TOKEN_CACHE_CAPACITY => {
                    let capacity = parse_u64(KEY_TOKEN_CACHE_CAPACITY, value)?;
                    config.token_cache_capacity = usize::try_from(capacity).map_err(|_| {
                        out_of_range(
                            KEY_TOKEN_CACHE_CAPACITY,
                            capacity,
                            "a value that fits the platform pointer width",
                        )
                    })?;
                }
                // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-05
                // An unknown key fails validation and is named in the error.
                other => return Err(unknown_key(other)),
                // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-05
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-04
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-04
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-01

        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-07
        config.validate()?;
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-07
        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-08
        // A validation rule returns the typed error naming the offending key.
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-08
        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-09
        Ok(config)
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-09
    }

    /// Validates the value ranges of the assembled configuration.
    ///
    /// # Errors
    /// Returns the typed configuration error naming the offending key when a
    /// timeout or the body limit is not strictly positive.
    pub fn validate(&self) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-cl-06
        // Validate value ranges: every timeout and the body limit must be
        // strictly greater than zero.
        for (key, value) in [
            (KEY_PROXY_TIMEOUT_SECS, self.proxy_timeout_secs),
            (KEY_TOKEN_CACHE_TTL_SECS, self.token_cache_ttl_secs),
            (KEY_BODY_LIMIT_BYTES, self.body_limit_bytes),
        ] {
            if value == 0 {
                return Err(out_of_range(
                    key,
                    value,
                    "a value strictly greater than zero",
                ));
            }
        }
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-cl-06
    }
}

/// Builds the typed error for an unknown configuration key.
fn unknown_key(key: &str) -> OagwError {
    OagwError::validation_error(format!(
        "oagw.config: unknown key '{key}' (known keys: {})",
        KNOWN_CONFIG_KEYS.join(", ")
    ))
}

/// Builds the typed error for an out-of-range configuration value.
fn out_of_range(key: &str, value: impl std::fmt::Display, expectation: &str) -> OagwError {
    OagwError::validation_error(format!(
        "oagw.config: '{key}' value {value} is out of range, expected {expectation}"
    ))
}

/// Parses a non-negative integer configuration value.
fn parse_u64(key: &str, value: &Value) -> Result<u64, OagwError> {
    value.as_u64().ok_or_else(|| {
        OagwError::validation_error(format!(
            "oagw.config: '{key}' must be a non-negative integer, got {value}"
        ))
    })
}

/// Parses a boolean configuration value.
fn parse_bool(key: &str, value: &Value) -> Result<bool, OagwError> {
    value.as_bool().ok_or_else(|| {
        OagwError::validation_error(format!(
            "oagw.config: '{key}' must be a boolean, got {value}"
        ))
    })
}

/// Parses the `ssrf_policy` object, rejecting unknown nested keys.
fn parse_ssrf_policy(key: &str, value: &Value) -> Result<SsrfPolicyConfig, OagwError> {
    let entries = value.as_object().ok_or_else(|| {
        OagwError::validation_error(format!(
            "oagw.config: '{key}' must be an object with an '{KEY_SSRF_ENABLED}' boolean"
        ))
    })?;

    let mut policy = SsrfPolicyConfig::default();
    for (nested, nested_value) in entries {
        if nested == KEY_SSRF_ENABLED {
            policy.enabled = parse_bool(&format!("{key}.{nested}"), nested_value)?;
        } else {
            return Err(OagwError::validation_error(format!(
                "oagw.config: unknown key '{key}.{nested}' (known keys: {KEY_SSRF_ENABLED})"
            )));
        }
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The `oagw.config` block of `/app/config/e2e-local.yaml`.
    fn e2e_block() -> Value {
        json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        })
    }

    fn load_json(value: Value) -> Result<OagwConfig, OagwError> {
        OagwConfig::load(Some(&value))
    }

    #[test]
    fn defaults_apply_when_the_block_is_absent() {
        assert_eq!(OagwConfig::load(None).unwrap(), OagwConfig::default());
    }

    #[test]
    fn defaults_apply_when_the_block_is_empty() {
        assert_eq!(load_json(json!({})).unwrap(), OagwConfig::default());
    }

    #[test]
    fn defaults_carry_the_pinned_values() {
        let config = OagwConfig::default();
        assert_eq!(config.proxy_timeout_secs, 30);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
        assert_eq!(config.body_limit_bytes, 100 * 1024 * 1024);
        assert_eq!(config.token_cache_ttl_secs, 300);
        assert_eq!(config.token_cache_capacity, 10_000);
    }

    #[test]
    fn overrides_replace_defaults_before_validation() {
        let config = load_json(json!({
            "proxy_timeout_secs": 5,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "body_limit_bytes": 1024,
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 7
        }))
        .unwrap();

        assert_eq!(config.proxy_timeout_secs, 5);
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert_eq!(config.body_limit_bytes, 1024);
        assert_eq!(config.token_cache_ttl_secs, 60);
        assert_eq!(config.token_cache_capacity, 7);
    }

    #[test]
    fn the_e2e_platform_block_is_accepted() {
        let config = load_json(e2e_block()).unwrap();
        assert_eq!(config.proxy_timeout_secs, 2);
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert_eq!(config.body_limit_bytes, DEFAULT_BODY_LIMIT_BYTES);
    }

    #[test]
    fn unknown_key_is_rejected_naming_the_key() {
        let error = load_json(json!({ "proxy_timeout_secs": 5, "unknown_key": 1 })).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.mapping().variant, "ValidationError");
        assert!(
            error.detail().contains("unknown key 'unknown_key'"),
            "detail must name the offending key, got: {}",
            error.detail()
        );
    }

    #[test]
    fn nested_unknown_key_in_ssrf_policy_is_rejected() {
        let error =
            load_json(json!({ "ssrf_policy": { "enabled": true, "extra": 1 } })).unwrap_err();
        assert!(
            error.detail().contains("unknown key 'ssrf_policy.extra'"),
            "detail must name the offending nested key, got: {}",
            error.detail()
        );
    }

    #[test]
    fn zero_proxy_timeout_is_rejected() {
        let error = load_json(json!({ "proxy_timeout_secs": 0 })).unwrap_err();
        assert!(
            error.detail().contains("'proxy_timeout_secs'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn negative_token_cache_ttl_is_rejected() {
        let error = load_json(json!({ "token_cache_ttl_secs": -1 })).unwrap_err();
        assert!(
            error.detail().contains("'token_cache_ttl_secs'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn zero_body_limit_is_rejected() {
        let error = load_json(json!({ "body_limit_bytes": 0 })).unwrap_err();
        assert!(
            error.detail().contains("'body_limit_bytes'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn type_mismatch_is_rejected_naming_the_key() {
        let error = load_json(json!({ "allow_http_upstream": "yes" })).unwrap_err();
        assert!(
            error.detail().contains("'allow_http_upstream'"),
            "{}",
            error.detail()
        );

        let error = load_json(json!({ "ssrf_policy": true })).unwrap_err();
        assert!(
            error.detail().contains("'ssrf_policy'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn non_object_block_is_rejected() {
        let error = OagwConfig::load(Some(&json!([1, 2]))).unwrap_err();
        assert!(error.detail().contains("oagw.config"), "{}", error.detail());
    }

    #[test]
    fn protocol_values_are_the_closed_set() {
        for name in UPSTREAM_PROTOCOLS {
            let parsed = UpstreamProtocol::parse(name).unwrap();
            assert_eq!(parsed.as_str(), *name);
        }
        assert_eq!(
            UPSTREAM_PROTOCOLS,
            &["http", "https", "wss", "grpc", "wt"],
            "http is a legal protocol value; plaintext establishment is governed by allow_http_upstream"
        );

        let error = UpstreamProtocol::parse("ftp").unwrap_err();
        assert!(error.detail().contains("'ftp'"), "{}", error.detail());
        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn the_key_set_is_closed() {
        assert_eq!(KNOWN_CONFIG_KEYS.len(), 6);
        let mut names: Vec<_> = KNOWN_CONFIG_KEYS.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            KNOWN_CONFIG_KEYS.len(),
            "duplicate key constant"
        );
    }

    #[test]
    fn serialization_emits_only_the_closed_key_set() {
        let value = serde_json::to_value(OagwConfig::default()).unwrap();
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        let mut expected: Vec<String> = KNOWN_CONFIG_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(value["ssrf_policy"].as_object().unwrap().len(), 1);
    }
}

// @cpt-end:cpt-cf-oagw-dod-config-surface:p1:inst-full
