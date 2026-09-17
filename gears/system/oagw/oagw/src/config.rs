//! OAGW gear configuration model.
//!
//! The config block is read from the platform config under `oagw.config`.
//! All fields carry serde defaults so an absent block produces a working
//! gear; validation is fail-loud (per `cpt-cf-oagw-principle-fail-loud-config`)
//! for values that would make the gear unusable (non-positive timeouts or
//! cache capacities).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default outbound proxy timeout in seconds when the config omits it.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 2;
/// Do not allow plaintext HTTP upstreams unless explicitly configured.
pub const DEFAULT_ALLOW_HTTP_UPSTREAM: bool = false;
/// SSRF protection is on by default (defense in depth).
pub const DEFAULT_SSRF_ENABLED: bool = true;
/// OAuth token cache TTL in seconds (default 5 minutes).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// OAuth token cache capacity in entries (default 10k).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

fn default_proxy_timeout_secs() -> u64 {
    DEFAULT_PROXY_TIMEOUT_SECS
}

fn default_allow_http_upstream() -> bool {
    DEFAULT_ALLOW_HTTP_UPSTREAM
}

fn default_ssrf_enabled() -> bool {
    DEFAULT_SSRF_ENABLED
}

fn default_token_cache_ttl_secs() -> u64 {
    DEFAULT_TOKEN_CACHE_TTL_SECS
}

fn default_token_cache_capacity() -> usize {
    DEFAULT_TOKEN_CACHE_CAPACITY
}

fn default_enabled() -> bool {
    true
}

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_path_prefix() -> String {
    String::new()
}

/// Failure-mode policy for the SSRF guard (deny private ranges and
/// non-public-suffix hostnames).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Whether the SSRF guard is active. Enabled by default.
    #[serde(default = "default_ssrf_enabled")]
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SSRF_ENABLED,
        }
    }
}

/// A config-seeded upstream (identical shape to the control-plane CRUD DTO).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSeed {
    /// Unique alias for the upstream (also used as the proxy path segment).
    pub alias: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// Upstream host (DNS name or literal IP).
    pub host: String,
    /// Upstream TCP port (defaults per scheme when omitted).
    #[serde(default)]
    pub port: Option<u16>,
    /// `https` (default) or `http`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Optional path prefix prepended to proxied requests.
    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// A config-seeded route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSeed {
    /// Unique route alias.
    pub alias: String,
    /// The upstream this route forwards to.
    pub upstream_alias: String,
    /// Optional allowed HTTP methods.
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitSeed>,
    /// Route-level CORS policy.
    #[serde(default)]
    pub cors: Option<CorsSeed>,
    /// Whether the route is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Route-level rate-limit seed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSeed {
    /// Token-bucket capacity (burst).
    #[serde(default)]
    pub capacity: u64,
    /// Token refill rate per second.
    #[serde(default)]
    pub refill_per_sec: f64,
}

/// Route-level CORS seed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CorsSeed {
    /// Whether CORS handling is enabled for the route.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins (`*` for any).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    /// Allowed request headers.
    #[serde(default)]
    pub allowed_headers: Vec<String>,
    /// Exposed response headers.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Preflight max-age in seconds.
    #[serde(default)]
    pub max_age_secs: u64,
    /// Reflect credentials.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// A config-seeded plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSeed {
    /// Unique plugin alias.
    pub alias: String,
    /// Plugin kind (noop, apikey, oauth2_client_cred, required_headers, ...).
    pub kind: String,
    /// Whether the plugin is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// JSON configuration for the plugin.
    #[serde(default)]
    pub config: serde_json::Value,
    /// Upstream aliases the plugin is bound to.
    #[serde(default)]
    pub upstreams: Vec<String>,
    /// Route aliases the plugin is bound to.
    #[serde(default)]
    pub routes: Vec<String>,
}

/// Root OAGW configuration, deserialized from the platform config block.
///
/// Fields not present in the config fall back to the defaults above, keeping
/// the gear permissive out of the box while remaining loudly configurable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Outbound proxy timeout (seconds), enforced by the data plane.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Permit plaintext `http://` upstreams (disabled by default).
    #[serde(default = "default_allow_http_upstream")]
    pub allow_http_upstream: bool,
    /// SSRF guard policy.
    pub ssrf_policy: SsrfPolicyConfig,
    /// OAuth token cache TTL (seconds).
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,
    /// OAuth token cache capacity (entries).
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
    /// Config-seeded upstreams.
    #[serde(default)]
    pub upstreams: Vec<UpstreamSeed>,
    /// Config-seeded routes.
    #[serde(default)]
    pub routes: Vec<RouteSeed>,
    /// Config-seeded plugins.
    #[serde(default)]
    pub plugins: Vec<PluginSeed>,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: DEFAULT_ALLOW_HTTP_UPSTREAM,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            upstreams: Vec::new(),
            routes: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

impl OagwConfig {
    /// Validates the config, failing loudly on values that would make the
    /// gear unusable (non-positive timeouts / capacities). Unknown upstream
    /// schemes or unresolved config-seeded references are also rejected so
    /// misconfiguration surfaces at startup rather than at request time.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first invalid setting.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.proxy_timeout_secs == 0 {
            anyhow::bail!(
                "oagw: proxy_timeout_secs must be > 0 (got {})",
                self.proxy_timeout_secs
            );
        }
        if self.token_cache_ttl_secs == 0 {
            anyhow::bail!(
                "oagw: token_cache_ttl_secs must be > 0 (got {})",
                self.token_cache_ttl_secs
            );
        }
        if self.token_cache_capacity == 0 {
            anyhow::bail!(
                "oagw: token_cache_capacity must be > 0 (got {})",
                self.token_cache_capacity
            );
        }
        for u in &self.upstreams {
            if u.alias.trim().is_empty() || u.host.trim().is_empty() {
                anyhow::bail!("oagw: config-seeded upstream requires non-empty `alias` and `host`");
            }
            Self::validate_scheme(&u.scheme, self.allow_http_upstream)?;
        }
        for r in &self.routes {
            if r.alias.trim().is_empty() {
                anyhow::bail!("oagw: config-seeded route requires a non-empty `alias`");
            }
            if self.upstreams.iter().all(|u| u.alias != r.upstream_alias) {
                anyhow::bail!(
                    "oagw: config-seeded route `{}` references unknown upstream `{}`",
                    r.alias,
                    r.upstream_alias
                );
            }
        }
        for p in &self.plugins {
            if p.alias.trim().is_empty() {
                anyhow::bail!("oagw: config-seeded plugin requires a non-empty `alias`");
            }
            for u in &p.upstreams {
                if self.upstreams.iter().all(|x| &x.alias != u) {
                    anyhow::bail!(
                        "oagw: config-seeded plugin `{}` references unknown upstream `{}`",
                        p.alias,
                        u
                    );
                }
            }
            for r in &p.routes {
                if self.routes.iter().all(|x| &x.alias != r) {
                    anyhow::bail!(
                        "oagw: config-seeded plugin `{}` references unknown route `{}`",
                        p.alias,
                        r
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_scheme(scheme: &str, allow_http: bool) -> anyhow::Result<()> {
        match scheme {
            "https" => Ok(()),
            "http" if allow_http => Ok(()),
            "http" => anyhow::bail!(
                "oagw: upstream scheme `http` is disabled (allow_http_upstream=false)"
            ),
            other => anyhow::bail!("oagw: unknown upstream scheme `{other}` (expected https|http)"),
        }
    }

    /// Route-level rate limit as a `Duration`-friendly pair, or `None`.
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_permissive_and_valid() {
        let cfg = OagwConfig::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert!(cfg.ssrf_policy.enabled);
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.token_cache_ttl_secs, DEFAULT_TOKEN_CACHE_TTL_SECS);
        assert_eq!(cfg.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
    }

    #[test]
    fn zero_timeouts_fail_loud() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("proxy_timeout_secs"));

        let cfg = OagwConfig {
            token_cache_ttl_secs: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("token_cache_ttl_secs"));

        let cfg = OagwConfig {
            token_cache_capacity: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("token_cache_capacity"));
    }

    #[test]
    fn deserializes_with_defaults_from_partial_block() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 7,
            "allow_http_upstream": true,
        });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("partial block deserializes");
        assert_eq!(cfg.proxy_timeout_secs, 7);
        assert!(cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
    }

    /// Fail-loud config-boot contract: an unknown top-level or nested key
    /// must be rejected at parse time rather than silently ignored
    /// (FEATURE inst-parse, `deny_unknown_fields`).
    #[test]
    fn unknown_config_keys_fail_loud() {
        let raw = serde_json::json!({ "bogus_top_level": 1 });
        let err = serde_json::from_value::<OagwConfig>(raw).expect_err("unknown key must fail");
        assert!(err.to_string().contains("bogus_top_level"), "{err}");

        let raw = serde_json::json!({
            "ssrf_policy": { "enabled": true, "bogus_nested": 2 },
        });
        let err =
            serde_json::from_value::<OagwConfig>(raw).expect_err("unknown nested key must fail");
        assert!(err.to_string().contains("bogus_nested"), "{err}");
    }
}
