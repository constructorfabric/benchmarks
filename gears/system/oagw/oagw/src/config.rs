//! `OagwConfig` — the configuration surface the `oagw` gear loads at init.
//!
//! Realizes `cpt-cf-oagw-algo-config-load-validate` and
//! `cpt-cf-oagw-dod-config-surface`: exactly the five configurable families
//! tabulated in the feature, the declared defaults applied when
//! `oagw.config` is absent, unknown keys rejected, and out-of-range integers
//! rejected with the offending key named.

use serde::{Deserialize, Serialize};

use crate::domain::scheme::Scheme;

/// `proxy_timeout_secs` — at least 1.
pub const MIN_PROXY_TIMEOUT_SECS: u64 = 1;
/// `token_cache_ttl_secs` — at least 1.
pub const MIN_TOKEN_CACHE_TTL_SECS: u64 = 1;
/// `token_cache_capacity` — at least 1.
pub const MIN_TOKEN_CACHE_CAPACITY: u64 = 1;

/// Declared default for `proxy_timeout_secs`: the platform REST request
/// deadline applied by the built-in API gateway middleware stack, so an
/// omitted key behaves like the platform default rather than like an
/// unbounded request.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Declared default for `allow_http_upstream`: HTTPS-only posture.
pub const DEFAULT_ALLOW_HTTP_UPSTREAM: bool = false;
/// Declared default for `token_cache_ttl_secs` (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Declared default for `token_cache_capacity` (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: u64 = 10_000;

/// `proxy_timeout_secs` configuration key.
pub const KEY_PROXY_TIMEOUT_SECS: &str = "proxy_timeout_secs";
/// `allow_http_upstream` configuration key.
pub const KEY_ALLOW_HTTP_UPSTREAM: &str = "allow_http_upstream";
/// `ssrf_policy` configuration key.
pub const KEY_SSRF_POLICY: &str = "ssrf_policy";
/// `token_cache_ttl_secs` configuration key.
pub const KEY_TOKEN_CACHE_TTL_SECS: &str = "token_cache_ttl_secs";
/// `token_cache_capacity` configuration key.
pub const KEY_TOKEN_CACHE_CAPACITY: &str = "token_cache_capacity";
/// `ssrf_policy.enabled` nested configuration key.
pub const KEY_SSRF_POLICY_ENABLED: &str = "ssrf_policy.enabled";

/// Every top-level key of the `oagw.config` surface.
pub const KNOWN_KEYS: [&str; 5] = [
    KEY_PROXY_TIMEOUT_SECS,
    KEY_ALLOW_HTTP_UPSTREAM,
    KEY_SSRF_POLICY,
    KEY_TOKEN_CACHE_TTL_SECS,
    KEY_TOKEN_CACHE_CAPACITY,
];

/// Configuration error naming the offending configuration key.
///
/// Realizes the error half of `cpt-cf-oagw-algo-config-load-validate`: init
/// aborts with this error, the gear does not register, and startup fails fast
/// before any later feature can consume a half-configured gear.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A key that is not part of the configuration surface was supplied.
    #[error("unknown configuration key '{key}' under oagw.config")]
    UnknownKey {
        /// The offending key, as written in the configuration.
        key: String,
    },
    /// An integer key fell below its declared minimum.
    #[error("configuration key '{key}' must be an integer of at least {minimum}, got {actual}")]
    OutOfRange {
        /// The offending integer key.
        key: String,
        /// The smallest accepted value.
        minimum: u64,
        /// The supplied value.
        actual: u64,
    },
    /// The section could not be parsed into the configuration surface.
    #[error("invalid oagw.config section: {message}")]
    Deserialize {
        /// Human-readable parse failure, including the offending key.
        message: String,
    },
}

impl ConfigError {
    /// The offending configuration key, when the error is attributable to one.
    #[must_use]
    pub fn offending_key(&self) -> Option<&str> {
        match self {
            Self::UnknownKey { key } | Self::OutOfRange { key, .. } => Some(key),
            Self::Deserialize { .. } => None,
        }
    }
}

/// SSRF enforcement posture.
///
/// `ssrf_policy.enabled` gates the SSRF enforcement owned by the data-plane
/// proxy feature; this feature only carries and validates the key and
/// evaluates no SSRF rule of its own. The fail-safe default is `true`:
/// enforcement is on until an operator turns it off.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Whether SSRF enforcement is active.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-defaults
        Self {
            enabled: DEFAULT_SSRF_POLICY_ENABLED,
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-defaults
    }
}

/// Fail-safe default for `ssrf_policy.enabled`.
pub const DEFAULT_SSRF_POLICY_ENABLED: bool = true;

// @cpt-dod:cpt-cf-oagw-dod-config-surface:p1
/// Configuration surface of the `oagw` gear.
///
/// Loaded through the platform configuration provider
/// (`ctx.config_or_default::<OagwConfig>()`), which yields
/// [`OagwConfig::default`] when the `oagw.config` section is absent. Every
/// integer key is validated against its declared minimum by
/// [`OagwConfig::validate`].
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Outbound request deadline in seconds. At least 1.
    pub proxy_timeout_secs: u64,
    /// The only input that admits the `http` endpoint scheme literal at write
    /// time. Recording the lifted posture does not authorize a plaintext dial.
    pub allow_http_upstream: bool,
    /// SSRF enforcement posture; see [`SsrfPolicy`].
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for a cached access-token TTL in seconds (ADR 0008). At least 1.
    pub token_cache_ttl_secs: u64,
    /// Maximum token-cache entries (ADR 0008). At least 1.
    pub token_cache_capacity: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-defaults
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: DEFAULT_ALLOW_HTTP_UPSTREAM,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-defaults
    }
}

impl OagwConfig {
    /// Parses and validates the raw `oagw.config` mapping.
    ///
    /// `None` (the section is absent entirely) yields the declared defaults
    /// for every key. This is the single entry point of
    /// `cpt-cf-oagw-algo-config-load-validate`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownKey`] for a key outside the surface,
    /// [`ConfigError::OutOfRange`] for an integer below its declared minimum,
    /// and [`ConfigError::Deserialize`] for a value of the wrong type.
    pub fn load(raw: Option<&serde_json::Value>) -> Result<Self, ConfigError> {
        let config = match raw {
            None => Self::default(),
            Some(value) => Self::from_value(value)?,
        };
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-fail-if
        config.validate()?;
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-fail-if

        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-return
        Ok(config)
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-return
    }

    /// Parses the raw `oagw.config` mapping, applying the declared default
    /// for every absent key.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownKey`] for a key outside the surface and
    /// [`ConfigError::Deserialize`] for a value of the wrong type. Range
    /// checking is left to [`OagwConfig::validate`].
    pub fn from_value(raw: &serde_json::Value) -> Result<Self, ConfigError> {
        reject_unknown_keys(raw)?;
        check_raw_integer(raw, KEY_PROXY_TIMEOUT_SECS, MIN_PROXY_TIMEOUT_SECS)?;
        check_raw_integer(raw, KEY_TOKEN_CACHE_TTL_SECS, MIN_TOKEN_CACHE_TTL_SECS)?;
        check_raw_integer(raw, KEY_TOKEN_CACHE_CAPACITY, MIN_TOKEN_CACHE_CAPACITY)?;

        serde_json::from_value(raw.clone()).map_err(|e| ConfigError::Deserialize {
            message: e.to_string(),
        })
    }

    /// Validates the integer keys against their declared minimums.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OutOfRange`] naming the offending key for the
    /// first integer key below its declared minimum.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-int-loop
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-int-check
        check_min(
            KEY_PROXY_TIMEOUT_SECS,
            self.proxy_timeout_secs,
            MIN_PROXY_TIMEOUT_SECS,
        )?;
        check_min(
            KEY_TOKEN_CACHE_TTL_SECS,
            self.token_cache_ttl_secs,
            MIN_TOKEN_CACHE_TTL_SECS,
        )?;
        check_min(
            KEY_TOKEN_CACHE_CAPACITY,
            self.token_cache_capacity,
            MIN_TOKEN_CACHE_CAPACITY,
        )?;
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-int-check
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-int-loop
    }

    /// Write-time scheme admission delegated to the [`Scheme`] value object.
    ///
    /// `allow_http_upstream` is the only input that can change the outcome for
    /// `Scheme::Http`; no other configuration key affects any scheme.
    #[must_use]
    pub const fn admits_scheme(&self, scheme: Scheme) -> bool {
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-http-record
        scheme.is_write_admitted(self.allow_http_upstream)
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-http-record
    }
}

/// Rejects keys that are not part of the configuration surface, at the top
/// level and inside `ssrf_policy`.
fn reject_unknown_keys(raw: &serde_json::Value) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-parse
    let Some(fields) = raw.as_object() else {
        return Ok(());
    };

    for key in fields.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(ConfigError::UnknownKey { key: key.clone() });
        }
    }

    if let Some(ssrf) = fields.get(KEY_SSRF_POLICY)
        && let Some(ssrf_fields) = ssrf.as_object()
    {
        for key in ssrf_fields.keys() {
            if key != "enabled" {
                return Err(ConfigError::UnknownKey {
                    key: format!("{KEY_SSRF_POLICY}.{key}"),
                });
            }
        }
    }

    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-parse
}

/// Range-checks a raw integer key before serde ever sees it, so the error
/// names the key even when the supplied value is not a `u64` at all.
fn check_raw_integer(raw: &serde_json::Value, key: &str, minimum: u64) -> Result<(), ConfigError> {
    let Some(found) = raw.get(key) else {
        return Ok(());
    };

    let Some(actual) = found.as_u64() else {
        return Err(ConfigError::Deserialize {
            message: format!("{key}: expected an integer of at least {minimum}, got {found}"),
        });
    };

    if actual < minimum {
        return Err(ConfigError::OutOfRange {
            key: key.to_owned(),
            minimum,
            actual,
        });
    }

    Ok(())
}

/// Range-checks one validated integer key.
fn check_min(key: &str, actual: u64, minimum: u64) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-fail-return
    if actual < minimum {
        return Err(ConfigError::OutOfRange {
            key: key.to_owned(),
            minimum,
            actual,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-fail-return
    Ok(())
}
