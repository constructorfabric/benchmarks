//! Gear-level configuration for the OAGW outbound gateway.

use serde::{Deserialize, Serialize};

/// Default value for [`OagwConfig::proxy_timeout_secs`].
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default value for [`TokenCacheConfig::ttl_secs`].
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default value for [`TokenCacheConfig::capacity`].
pub const DEFAULT_TOKEN_CACHE_CAPACITY: u64 = 1024;

fn default_true() -> bool {
    true
}

/// Policy governing which outbound targets the gear is willing to contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// When true, upstream hosts are validated against the deny list and
    /// loopback/link-local/private ranges are refused before a connection is made.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hosts that may never be contacted even when the policy is enabled.
    #[serde(default)]
    pub deny_hosts: Vec<String>,
    /// Hosts explicitly exempted from the policy.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_hosts: Vec::new(),
            allow_hosts: Vec::new(),
        }
    }
}

/// Cache settings for `OAuth2` client-credentials tokens obtained from an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// How long a cached token stays valid, in seconds.
    pub ttl_secs: u64,
    /// Maximum number of tokens held at once.
    pub capacity: u64,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

/// Gear-level configuration, deserialized from the `oagw.config` key of the runtime
/// configuration. Every field has a safe default so an absent configuration block still
/// yields a working, conservative gear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Maximum time a proxied request may spend talking to the upstream.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext (`http:` / `ws:`) upstream connections are permitted.
    pub allow_http_upstream: bool,
    /// Server-side request forgery policy applied before a connection is opened.
    pub ssrf_policy: SsrfPolicy,
    /// `OAuth2` token cache settings.
    pub token_cache: TokenCacheConfig,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache: TokenCacheConfig::default(),
        }
    }
}

impl OagwConfig {
    /// Returns the proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Returns true when a connection to `scheme` may actually be opened.
    ///
    /// The scheme field of an endpoint always accepts `http` and `ws`; this flag governs
    /// only whether a plaintext connection is made.
    #[must_use]
    pub fn permits_plaintext(&self, scheme: &str) -> bool {
        if matches!(scheme, "http" | "ws") {
            self.allow_http_upstream
        } else {
            true
        }
    }
}
