//! `OAuth2` token cache settings (ADR-0008).

use std::time::Duration;

/// Tunables of the `OAuth2` client-credentials token cache.
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached token's lifetime.
    pub ttl: Duration,
    /// Maximum number of cached entries.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_mins(5),
            capacity: 10_000,
        }
    }
}

impl TokenCacheConfig {
    /// Cache settings derived from the gear configuration.
    #[must_use]
    pub fn from_gear_config(config: &crate::config::OagwConfig) -> Self {
        Self {
            ttl: Duration::from_secs(config.token_cache_ttl_secs),
            capacity: usize::try_from(config.token_cache_capacity).unwrap_or(10_000),
        }
    }
}
