//! Typed binding of the `gears.oagw.config` configuration section.
//!
//! The struct mirrors the key set fixed by
//! `cpt-cf-oagw-algo-config-load`/`cpt-cf-oagw-dod-config-struct`:
//!
//! | Key | Recorded default | Bound |
//! |-----|------------------|-------|
//! | `proxy_timeout_secs` | `30` | strictly positive |
//! | `allow_http_upstream` | `false` | — |
//! | `ssrf_policy.enabled` | `true` | — |
//! | `token_cache_ttl_secs` | `300` | strictly greater than `30` |
//! | `token_cache_capacity` | `10000` | strictly positive |
//!
//! The surface carries no secret material: credentials stay behind `cred://`
//! references (`cpt-cf-oagw-principle-cred-isolation`), so nothing here has to
//! be redacted from the startup log line.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration section path, as it appears in the runtime configuration.
///
/// Repeated verbatim in every startup error so the operator can find the
/// offending key in the runtime configuration file.
pub const CONFIG_SECTION: &str = "gears.oagw.config";

/// Recorded default for [`OagwConfig::proxy_timeout_secs`].
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Recorded default for [`OagwConfig::allow_http_upstream`].
pub const DEFAULT_ALLOW_HTTP_UPSTREAM: bool = false;
/// Recorded default for [`SsrfPolicy::enabled`].
pub const DEFAULT_SSRF_POLICY_ENABLED: bool = true;
/// Recorded default for [`OagwConfig::token_cache_ttl_secs`].
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Recorded default for [`OagwConfig::token_cache_capacity`].
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// `proxy_timeout_secs` bound: strictly positive.
pub const MIN_PROXY_TIMEOUT_SECS: u64 = 1;
/// `token_cache_ttl_secs` bound: strictly greater than 30 seconds.
pub const MIN_TOKEN_CACHE_TTL_SECS: u64 = 30;
/// `token_cache_capacity` bound: strictly positive.
pub const MIN_TOKEN_CACHE_CAPACITY: usize = 1;

/// `gears.oagw.config` key: upstream proxy timeout, in seconds.
pub const KEY_PROXY_TIMEOUT_SECS: &str = "proxy_timeout_secs";
/// `gears.oagw.config` key: plaintext upstream opt-in.
pub const KEY_ALLOW_HTTP_UPSTREAM: &str = "allow_http_upstream";
/// `gears.oagw.config` key: SSRF policy block.
pub const KEY_SSRF_POLICY: &str = "ssrf_policy";
/// `gears.oagw.config` key: token-cache entry time-to-live, in seconds.
pub const KEY_TOKEN_CACHE_TTL_SECS: &str = "token_cache_ttl_secs";
/// `gears.oagw.config` key: token-cache entry capacity.
pub const KEY_TOKEN_CACHE_CAPACITY: &str = "token_cache_capacity";

/// Every key the gear reads from `gears.oagw.config`, in declaration order.
pub const KNOWN_KEYS: [&str; 5] = [
    KEY_PROXY_TIMEOUT_SECS,
    KEY_ALLOW_HTTP_UPSTREAM,
    KEY_SSRF_POLICY,
    KEY_TOKEN_CACHE_TTL_SECS,
    KEY_TOKEN_CACHE_CAPACITY,
];

/// Expected shape of a key, used verbatim in the startup error.
const EXPECTED_PROXY_TIMEOUT: &str = "an integer number of seconds greater than 0";
/// Expected shape of a key, used verbatim in the startup error.
const EXPECTED_ALLOW_HTTP: &str = "a boolean";
/// Expected shape of a key, used verbatim in the startup error.
const EXPECTED_SSRF_POLICY: &str = "an object with an optional boolean `enabled` field";
/// Expected shape of a key, used verbatim in the startup error.
const EXPECTED_TOKEN_CACHE_TTL: &str = "an integer number of seconds greater than 30";
/// Expected shape of a key, used verbatim in the startup error.
const EXPECTED_TOKEN_CACHE_CAPACITY: &str = "an integer number of cache entries greater than 0";

/// SSRF posture of the proxy: whether upstream host validation is enforced.
///
/// The key is part of the runtime configuration surface (DECOMPOSITION
/// assumption 7). The recorded default keeps host validation unconditional;
/// only an explicit `false` relaxes it, so the default posture fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Validate upstream hosts against the configured endpoint set.
    #[serde(default = "default_ssrf_enabled")]
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SSRF_POLICY_ENABLED,
        }
    }
}

/// Recorded default for `ssrf_policy.enabled`.
fn default_ssrf_enabled() -> bool {
    DEFAULT_SSRF_POLICY_ENABLED
}

/// Configuration error for the `gears.oagw.config` section.
///
/// Always names the offending key (or the whole section when the section itself
/// is unusable) and the expected shape, so a failed `init` aborts host startup
/// with an actionable message instead of running with a partially applied
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Offending key, or [`CONFIG_SECTION`] when the section itself is unusable.
    key: String,
    /// What the key is expected to hold.
    expected: String,
}

impl ConfigError {
    pub(crate) fn new(key: impl Into<String>, expected: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            expected: expected.into(),
        }
    }

    /// Key the error is about.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Expected shape of the offending key.
    #[must_use]
    pub fn expected(&self) -> &str {
        &self.expected
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.key == CONFIG_SECTION {
            write!(
                f,
                "invalid `{}`: expected {}",
                CONFIG_SECTION, self.expected
            )
        } else {
            write!(
                f,
                "invalid `{}.{}`: expected {}",
                CONFIG_SECTION, self.key, self.expected
            )
        }
    }
}

impl std::error::Error for ConfigError {}

// @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-03
/// OAGW gear configuration, bound to the `gears.oagw.config` keys.
///
/// Absent keys take the recorded defaults (`#[serde(default)]` on the container
/// plus the recorded-default `Default` impl); a present key that cannot be
/// deserialized into its typed field, or that violates its recorded bound,
/// fails gear `init`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Upstream request timeout, in seconds (`proxy_timeout_secs`).
    pub proxy_timeout_secs: u64,
    /// Admit a plaintext (`http`) upstream connection (`allow_http_upstream`).
    ///
    /// The default keeps the HTTPS-only posture of
    /// `cpt-cf-oagw-constraint-https-only`; an operator must opt in explicitly.
    pub allow_http_upstream: bool,
    /// Upstream host validation posture (`ssrf_policy.enabled`).
    pub ssrf_policy: SsrfPolicy,
    /// Token-cache entry time-to-live, in seconds (`token_cache_ttl_secs`).
    pub token_cache_ttl_secs: u64,
    /// Token-cache entry capacity (`token_cache_capacity`).
    pub token_cache_capacity: usize,
}
// @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-03

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: DEFAULT_ALLOW_HTTP_UPSTREAM,
            ssrf_policy: SsrfPolicy {
                enabled: DEFAULT_SSRF_POLICY_ENABLED,
            },
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-config-struct:p1:inst-full
impl OagwConfig {
    /// Load the configuration from the raw `gears.oagw.config` section.
    ///
    /// Absent sections (an empty or `null` value) yield the recorded defaults;
    /// a section that fails to deserialize is diagnosed key by key so the
    /// returned [`ConfigError`] names the offending key.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key and its expected
    /// shape when the section cannot be deserialized, or when a value violates
    /// its recorded bound.
    pub fn from_section(section: &Value) -> Result<Self, ConfigError> {
        // Absent section: the recorded defaults are the effective configuration.
        if section.is_null() {
            return Ok(Self::default());
        }

        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-04
        // Deserialize the whole section; `deny_unknown_fields` plus the
        // container-level `default` turn an absent key into its recorded
        // default and an unknown key into a deserialization failure.
        match serde_json::from_value::<Self>(section.clone()) {
            // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-05
            Ok(config) => {
                config.validate()?;
                Ok(config)
            }
            // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-05
            // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-06
            // A present key failed: re-read the section key by key so the
            // error names the offending key instead of a serde path.
            Err(source) => {
                // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-07
                let (key, expected) = Self::diagnose(section);
                let mut error = ConfigError::new(key, expected);
                error.expected = format!("{} (serde: {source})", error.expected);
                Err(error)
                // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-07
            }
            // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-06
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-04
    }

    /// Explain why a section is unusable, naming the offending key.
    ///
    /// Returns `None` when the section deserializes cleanly; used by gear
    /// `init` to enrich a config-provider failure with the offending key.
    #[must_use]
    pub fn describe_section_error(section: &Value) -> Option<ConfigError> {
        Self::from_section(section).err()
    }

    /// Validate the recorded bounds of the loaded values.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key when the proxy
    /// timeout, the token-cache TTL or the token-cache capacity violates its
    /// recorded bound.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.proxy_timeout_secs < MIN_PROXY_TIMEOUT_SECS {
            return Err(ConfigError::new(
                KEY_PROXY_TIMEOUT_SECS,
                format!("{EXPECTED_PROXY_TIMEOUT}, got {}", self.proxy_timeout_secs),
            ));
        }
        if self.token_cache_ttl_secs <= MIN_TOKEN_CACHE_TTL_SECS {
            return Err(ConfigError::new(
                KEY_TOKEN_CACHE_TTL_SECS,
                format!("{EXPECTED_TOKEN_CACHE_TTL}, got {}", self.token_cache_ttl_secs),
            ));
        }
        if self.token_cache_capacity < MIN_TOKEN_CACHE_CAPACITY {
            return Err(ConfigError::new(
                KEY_TOKEN_CACHE_CAPACITY,
                format!(
                    "{EXPECTED_TOKEN_CACHE_CAPACITY}, got {}",
                    self.token_cache_capacity
                ),
            ));
        }
        Ok(())
    }

    /// Read the section key by key and return `(offending key, expected shape)`.
    ///
    /// Used only on the failure path of `from_section`: the whole-section
    /// deserialization already failed, so exactly one of these diagnoses is the
    /// cause.
    fn diagnose(section: &Value) -> (String, String) {
        if !section.is_object() {
            return (
                CONFIG_SECTION.to_owned(),
                format!("an object with the keys {}", join_known_keys()),
            );
        }

        // @cpt-begin:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-02
        // Present key that does not deserialize into its typed field: the same
        // key set the load path walks, probed with the type of its recorded
        // field so the failure names the offending key.
        for key in KNOWN_KEYS {
            let Some(raw) = section.get(key) else {
                continue;
            };
            let key_holds_expected_shape = match key {
                KEY_PROXY_TIMEOUT_SECS | KEY_TOKEN_CACHE_TTL_SECS => {
                    serde_json::from_value::<u64>(raw.clone()).is_ok()
                }
                KEY_ALLOW_HTTP_UPSTREAM => serde_json::from_value::<bool>(raw.clone()).is_ok(),
                KEY_SSRF_POLICY => serde_json::from_value::<SsrfPolicy>(raw.clone()).is_ok(),
                KEY_TOKEN_CACHE_CAPACITY => serde_json::from_value::<usize>(raw.clone()).is_ok(),
                _ => true,
            };
            if !key_holds_expected_shape {
                return (key.to_owned(), expected_shape_of(key).to_owned());
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load:p1:inst-gf-cfg-02

        // Unknown key: `deny_unknown_fields` rejected it.
        if let Some(key) = section
            .as_object()
            .and_then(|object| {
                object
                    .keys()
                    .find(|key| !KNOWN_KEYS.contains(&key.as_str()))
                    .cloned()
            })
        {
            return (
                key,
                format!("a known key (one of {})", join_known_keys()),
            );
        }

        // Nothing specific found: the section as a whole is unusable.
        (
            CONFIG_SECTION.to_owned(),
            format!("an object with the keys {}", join_known_keys()),
        )
    }
}
// @cpt-end:cpt-cf-oagw-dod-config-struct:p1:inst-full

/// Expected shape of a single key, for the startup error message.
fn expected_shape_of(key: &str) -> &'static str {
    match key {
        KEY_PROXY_TIMEOUT_SECS => EXPECTED_PROXY_TIMEOUT,
        KEY_ALLOW_HTTP_UPSTREAM => EXPECTED_ALLOW_HTTP,
        KEY_SSRF_POLICY => EXPECTED_SSRF_POLICY,
        KEY_TOKEN_CACHE_TTL_SECS => EXPECTED_TOKEN_CACHE_TTL,
        KEY_TOKEN_CACHE_CAPACITY => EXPECTED_TOKEN_CACHE_CAPACITY,
        _ => "a known key",
    }
}

/// Comma-separated key list for error messages.
fn join_known_keys() -> String {
    KNOWN_KEYS
        .iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_the_recorded_table() {
        let config = OagwConfig::default();
        assert_eq!(config.proxy_timeout_secs, 30);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
        assert_eq!(config.token_cache_ttl_secs, 300);
        assert_eq!(config.token_cache_capacity, 10_000);
    }

    #[test]
    fn absent_section_yields_the_recorded_defaults() {
        for section in [json!({}), Value::Null] {
            let config =
                OagwConfig::from_section(&section).expect("absent section must not fail");
            assert_eq!(config, OagwConfig::default());
        }
    }

    #[test]
    fn partial_section_fills_only_the_absent_keys() {
        let config = OagwConfig::from_section(&json!({ "token_cache_capacity": 5 }))
            .expect("single-key section must load");
        assert_eq!(config.token_cache_capacity, 5);
        assert_eq!(config.proxy_timeout_secs, 30);
        assert_eq!(config.token_cache_ttl_secs, 300);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
    }

    #[test]
    fn every_recorded_key_is_accepted() {
        let section = json!({
            "proxy_timeout_secs": 7,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "token_cache_ttl_secs": 600,
            "token_cache_capacity": 20,
        });
        let config = OagwConfig::from_section(&section).expect("valid section");
        assert_eq!(config.proxy_timeout_secs, 7);
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert_eq!(config.token_cache_ttl_secs, 600);
        assert_eq!(config.token_cache_capacity, 20);
    }

    #[test]
    fn empty_ssrf_policy_block_takes_the_recorded_default() {
        let config = OagwConfig::from_section(&json!({ "ssrf_policy": {} }))
            .expect("empty ssrf_policy block must default");
        assert!(config.ssrf_policy.enabled);
    }

    #[test]
    fn unknown_key_is_rejected_and_named() {
        let section = json!({ "proxy_timeout_secs": 5, "proxy_timeouts": 5 });
        let error = OagwConfig::from_section(&section).expect_err("unknown key must fail");
        assert_eq!(error.key(), "proxy_timeouts");
        assert!(error.to_string().contains("`gears.oagw.config.proxy_timeouts`"));
    }

    #[test]
    fn wrong_type_names_the_offending_key() {
        let section = json!({ "allow_http_upstream": "yes" });
        let error = OagwConfig::from_section(&section).expect_err("wrong type must fail");
        assert_eq!(error.key(), "allow_http_upstream");
        assert!(error.to_string().contains("boolean"), "{error}");
    }

    #[test]
    fn wrong_nested_type_names_the_parent_key() {
        let section = json!({ "ssrf_policy": { "enabled": "no" } });
        let error = OagwConfig::from_section(&section).expect_err("wrong nested type must fail");
        assert_eq!(error.key(), "ssrf_policy");
    }

    #[test]
    fn null_value_for_a_present_key_names_the_key() {
        let section = json!({ "proxy_timeout_secs": null });
        let error = OagwConfig::from_section(&section).expect_err("null value must fail");
        assert_eq!(error.key(), "proxy_timeout_secs");
    }

    #[test]
    fn non_object_section_names_the_section() {
        let error = OagwConfig::from_section(&json!("oagw"))
            .expect_err("non-object section must fail");
        assert_eq!(error.key(), CONFIG_SECTION);
    }

    #[test]
    fn zero_proxy_timeout_violates_the_recorded_bound() {
        let section = json!({ "proxy_timeout_secs": 0 });
        let error = OagwConfig::from_section(&section).expect_err("bound violation must fail");
        assert_eq!(error.key(), KEY_PROXY_TIMEOUT_SECS);
        assert!(error.to_string().contains("greater than 0"), "{error}");
    }

    #[test]
    fn token_cache_ttl_at_the_bound_is_rejected() {
        for value in [0_u64, 30_u64] {
            let section = json!({ "token_cache_ttl_secs": value });
            let error = OagwConfig::from_section(&section)
                .expect_err("token-cache TTL bound violation must fail");
            assert_eq!(error.key(), KEY_TOKEN_CACHE_TTL_SECS);
            assert!(error.to_string().contains("greater than 30"), "{error}");
        }
    }

    #[test]
    fn token_cache_ttl_above_the_bound_is_accepted() {
        let section = json!({ "token_cache_ttl_secs": 31 });
        let config = OagwConfig::from_section(&section).expect("31 seconds is a valid TTL");
        assert_eq!(config.token_cache_ttl_secs, 31);
    }

    #[test]
    fn zero_token_cache_capacity_violates_the_recorded_bound() {
        let section = json!({ "token_cache_capacity": 0 });
        let error = OagwConfig::from_section(&section).expect_err("bound violation must fail");
        assert_eq!(error.key(), KEY_TOKEN_CACHE_CAPACITY);
    }

    #[test]
    fn validate_reports_each_bound_violation() {
        let config = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().expect_err("proxy timeout bound").key(),
            KEY_PROXY_TIMEOUT_SECS
        );

        let config = OagwConfig {
            token_cache_capacity: 0,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().expect_err("cache capacity bound").key(),
            KEY_TOKEN_CACHE_CAPACITY
        );

        let config = OagwConfig {
            token_cache_ttl_secs: 10,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().expect_err("cache ttl bound").key(),
            KEY_TOKEN_CACHE_TTL_SECS
        );

        OagwConfig::default()
            .validate()
            .expect("recorded defaults are in range");
    }

    #[test]
    fn describe_section_error_reports_none_for_a_valid_section() {
        assert!(OagwConfig::describe_section_error(&json!({})).is_none());
        assert!(OagwConfig::describe_section_error(&Value::Null).is_none());
        assert!(OagwConfig::describe_section_error(&json!({ "proxy_timeout_secs": 5 })).is_none());
    }

    #[test]
    fn describe_section_error_names_the_key() {
        let error = OagwConfig::describe_section_error(&json!({ "token_cache_ttl_secs": 1 }))
            .expect("invalid section must produce a diagnosis");
        assert_eq!(error.key(), KEY_TOKEN_CACHE_TTL_SECS);
        assert_eq!(error.key(), "token_cache_ttl_secs");
    }

    #[test]
    fn config_surface_carries_no_secret_material() {
        // `cpt-cf-oagw-principle-cred-isolation`: the config surface has no key
        // that could hold a credential or a resolved secret value; the recorded
        // key set is closed and every key is a posture/limit knob.
        let config = OagwConfig::default();
        let serialized = serde_json::to_value(&config).expect("config is serializable");
        let object = serialized.as_object().expect("config serializes to an object");
        assert_eq!(object.len(), KNOWN_KEYS.len());
        for key in KNOWN_KEYS {
            assert!(object.contains_key(key), "missing key {key}");
        }
    }
}
