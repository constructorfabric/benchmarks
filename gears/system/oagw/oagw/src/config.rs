//! Gear configuration, read from `oagw.config` in the host configuration.

use serde::Deserialize;

/// Plaintext (SSRF) policy switch.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Whether the SSRF policy layer is enabled.
    #[serde(default = "SsrfPolicy::default_enabled")]
    pub enabled: bool,
}

impl SsrfPolicy {
    fn default_enabled() -> bool {
        true
    }
}

/// Gear configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Outbound request timeout budget in seconds.
    pub proxy_timeout_secs: u64,
    /// Whether the gateway may open a plaintext upstream connection.
    pub allow_http_upstream: bool,
    /// SSRF policy layer switch.
    pub ssrf_policy: SsrfPolicy,
    /// Hard body limit in bytes; defaults to 100 MB.
    pub body_limit_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy { enabled: true },
            body_limit_bytes: 100_000_000,
        }
    }
}

impl OagwConfig {
    /// Outbound timeout budget.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Whether a plaintext (`http`) connection may actually be opened.
    ///
    /// This is deliberately separate from scheme *acceptance*: `http` is a
    /// legal value for the endpoint `scheme` field regardless of this flag,
    /// which governs only whether the gateway opens such a connection.
    #[must_use]
    pub fn permits_plaintext_connection(&self) -> bool {
        self.allow_http_upstream
    }

    /// Validates the configuration.
    ///
    /// # Errors
    ///
    /// Returns a message when a value is outside its permitted range.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero".to_owned());
        }
        if self.body_limit_bytes == 0 {
            return Err("body_limit_bytes must be greater than zero".to_owned());
        }
        if self.body_limit_bytes > 100_000_000 {
            return Err("body_limit_bytes must not exceed 100 MB".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
