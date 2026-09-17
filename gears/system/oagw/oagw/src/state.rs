// Created: 2026-09-03 by Constructor Tech
//! Shared runtime state of the OAGW gear.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;

use crate::config::OagwConfig;
use crate::model::ValidationContext;
use crate::oauth::TokenCache;
use crate::rate_limit::RateLimiter;
use crate::store::OagwStore;

/// Runtime state shared by the control plane and the data plane.
pub struct OagwState {
    /// Effective gear configuration.
    pub config: OagwConfig,
    /// Control-plane and data-plane store.
    pub store: OagwStore,
    /// Rate-limit counters.
    pub limiter: RateLimiter,
    /// OAuth2 access-token cache.
    pub token_cache: TokenCache,
    /// Upstream connection factory.
    pub connector: crate::upstream_client::UpstreamConnector,
    /// Credential store used to resolve `cred://` references.
    pub credstore: Option<Arc<dyn CredStoreClientV1>>,
    /// Tenant resolver used for hierarchical alias and config resolution.
    pub tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
}

impl OagwState {
    /// Builds a state from a configuration.
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self::with_clients(config, None, None)
    }

    /// Builds a state from a configuration and the resolved collaborators.
    #[must_use]
    pub fn with_clients(
        config: OagwConfig,
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    ) -> Self {
        let token_cache = TokenCache::new(config.token_cache_capacity, config.token_cache_ttl_secs);
        Self {
            config,
            store: OagwStore::default(),
            limiter: RateLimiter::new(),
            token_cache,
            connector: crate::upstream_client::UpstreamConnector::new(),
            credstore,
            tenant_resolver,
        }
    }

    /// The shared upstream connection factory.
    #[must_use]
    pub fn connector(&self) -> &crate::upstream_client::UpstreamConnector {
        &self.connector
    }

    /// The validation switches derived from the configuration.
    #[must_use]
    pub fn validation_context(&self) -> ValidationContext {
        ValidationContext {
            allow_http_upstream: self.config.allow_http_upstream,
        }
    }

    /// The overall request budget.
    #[must_use]
    pub fn request_timeout(&self) -> std::time::Duration {
        self.config.request_timeout()
    }

    /// The connection establishment budget.
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.config.connect_timeout()
    }

    /// The streaming budget applied once headers have arrived.
    #[must_use]
    pub fn idle_timeout(&self) -> std::time::Duration {
        self.config.idle_timeout()
    }

    /// The ancestor chain of a tenant, oldest first.
    ///
    /// Falls back to the tenant alone when no resolver is available.
    pub async fn tenant_chain(&self, sec: &toolkit_security::SecurityContext) -> Vec<uuid::Uuid> {
        let tenant_id = sec.subject_tenant_id();
        let Some(resolver) = &self.tenant_resolver else {
            return vec![tenant_id];
        };
        let options = tenant_resolver_sdk::GetAncestorsOptions::default();
        match resolver
            .get_ancestors(sec, tenant_resolver_sdk::TenantId(tenant_id), &options)
            .await
        {
            Ok(response) => {
                let mut chain: Vec<uuid::Uuid> =
                    response.ancestors.iter().map(|a| a.id.0).collect();
                chain.reverse();
                chain.push(tenant_id);
                chain
            }
            Err(error) => {
                tracing::debug!(%error, "tenant ancestor lookup failed; falling back to the tenant");
                vec![tenant_id]
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_expose_https_only_validation() {
        let state = OagwState::new(OagwConfig::default());
        assert!(!state.validation_context().allow_http_upstream);
        assert_eq!(state.request_timeout(), std::time::Duration::from_secs(30));
        assert_eq!(state.connect_timeout(), std::time::Duration::from_secs(30));
        assert_eq!(state.idle_timeout(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn tenant_chain_falls_back_to_the_tenant() {
        let state = OagwState::new(OagwConfig::default());
        let sec = toolkit_security::SecurityContext::anonymous();
        let id = sec.subject_tenant_id();
        assert_eq!(state.store.list_upstreams(id).len(), 0);
    }
}
