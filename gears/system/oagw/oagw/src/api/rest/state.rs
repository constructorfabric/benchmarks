//! Shared state handed to every handler through an axum extension.

use crate::config::OagwConfig;
use crate::infra::oauth2::TokenCache;
use crate::infra::proxy::RoundRobin;
use crate::infra::ratelimit::RateLimiter;
use crate::infra::store::Store;
use credstore_sdk::CredStoreClientV1;
use std::sync::Arc;
use toolkit_http::HttpClient;

/// Everything a handler needs to serve a request.
pub struct OagwState {
    /// Gear configuration.
    pub config: OagwConfig,
    /// Tenant-scoped configuration storage.
    pub store: Store,
    /// Outbound HTTP client.
    pub http: HttpClient,
    /// Per-instance rate limiters.
    pub rate_limiter: RateLimiter,
    /// Round-robin cursor for multi-endpoint pools.
    pub round_robin: RoundRobin,
    /// Credential store, when one is wired.
    pub cred_store: Option<Arc<dyn CredStoreClientV1>>,
    /// Cache of client-credentials tokens.
    pub token_cache: TokenCache,
}

impl std::fmt::Debug for OagwState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwState")
            .field("config", &self.config)
            .field("cred_store", &self.cred_store.is_some())
            .finish_non_exhaustive()
    }
}

impl OagwState {
    /// Build the shared state.
    #[must_use]
    pub fn new(
        config: OagwConfig,
        http: HttpClient,
        cred_store: Option<Arc<dyn CredStoreClientV1>>,
    ) -> Self {
        let config_capacity = config.token_cache_capacity;
        Self {
            config,
            store: Store::new(),
            http,
            rate_limiter: RateLimiter::new(),
            round_robin: RoundRobin::new(),
            cred_store,
            token_cache: TokenCache::new(config_capacity),
        }
    }
}
