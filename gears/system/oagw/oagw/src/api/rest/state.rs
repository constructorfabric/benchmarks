//! Shared handler state — the gear's management half on the wire.
//!
//! [`OagwState`] is the one value every management handler extracts: the
//! compiled configuration, the store, the management service, the cache the
//! write path advances, and the permission enforcer the platform resolved at
//! startup. It carries no `axum` extractor and no request-scoped value, so it
//! is built once per gear and shared through `Router::with_state`.

use std::sync::Arc;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::pep::PolicyEnforcer;
use tenant_resolver_sdk::TenantResolverClient;

use crate::config::OagwConfig;
use crate::control_plane::cache::{ControlPlaneCache, RateLimitCleanup};
use crate::data_plane::forward::OutboundClient;
use crate::control_plane::service::ManagementService;
use crate::store::OagwStore;

/// The management surface's shared state.
#[derive(Clone)]
pub struct OagwState {
    config: Arc<OagwConfig>,
    store: Arc<OagwStore>,
    service: Arc<ManagementService>,
    enforcer: Option<Arc<PolicyEnforcer>>,
    resolver: Option<Arc<dyn TenantResolverClient>>,
    cache: Arc<ControlPlaneCache>,
    /// The Data Plane L1 cache of `cpt-cf-oagw-algo-dp-cache`, one per process.
    dp_cache: crate::data_plane::DpCache,
    /// The shared outbound client of `cpt-cf-oagw-adr-state-management`, one
    /// connector per process.
    outbound: OutboundClient,
    /// The per-upstream round-robin counters of `cpt-cf-oagw-algo-endpoint-select`.
    round_robin: crate::data_plane::RoundRobin,
    /// The three plugin registries of `cpt-cf-oagw-feature-plugin-system`,
    /// built once at initialization and shared by every chain composition.
    registries: crate::plugins::PluginRegistries,
    /// The per-instance rate-limit registry
    /// `cpt-cf-oagw-feature-rate-limiting` charges every proxy request on, one
    /// per process, held for the data plane ADR 0006 assigns.
    rate_limits: crate::data_plane::SharedLimits,
    /// The in-process observation seam
    /// `cpt-cf-oagw-feature-observability` collects the twelve families and the
    /// audit stream into, one per process, holding no persisted state.
    observability: Arc<crate::data_plane::observability::Observability>,
}

impl OagwState {
    /// Assembles the state over a built service.
    #[must_use]
    pub fn new(
        config: Arc<OagwConfig>,
        store: Arc<OagwStore>,
        service: Arc<ManagementService>,
        enforcer: Option<Arc<PolicyEnforcer>>,
        resolver: Option<Arc<dyn TenantResolverClient>>,
        cache: Arc<ControlPlaneCache>,
    ) -> Self {
        let registries = Self::registries_for(None, &config);
        let dp_cache = crate::data_plane::DpCache::new();
        // The Data Plane flush registers with the cache the write path
        // advances, so one successful write invalidates in the same process
        // and before the write's response is produced.
        cache.register_dp_flush(Arc::new(dp_cache.clone()));
        let state = Self {
            config,
            store,
            service,
            enforcer,
            resolver,
            cache,
            dp_cache,
            outbound: OutboundClient::new(),
            round_robin: crate::data_plane::RoundRobin::new(),
            registries,
            rate_limits: crate::data_plane::SharedLimits::new(),
            observability: Arc::new(
                crate::data_plane::observability::Observability::new(),
            ),
        };
        // The rate-limit cleanup registers with the same deletion seam the
        // data-plane flush does, so one successful upstream or route deletion
        // drops its counters before the delete's response is produced.
        let observer = Arc::new(crate::data_plane::RegistryCleanup::new(
            state.rate_limits.clone(),
        ));
        state.register_deletion_observer(observer);
        state
    }

    /// The registries a deployment serves, over the credential store the hub
    /// resolved or over the unavailable one.
    fn registries_for(
        cred_store: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        config: &OagwConfig,
    ) -> crate::plugins::PluginRegistries {
        crate::plugins::PluginRegistries::for_deployment(
            cred_store,
            crate::plugins::TokenCacheConfig::new(
                std::time::Duration::from_secs(config.token_cache_ttl_secs),
                config.token_cache_capacity as usize,
            ),
        )
    }

    /// The configuration the gear loaded at init.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The store the management service owns.
    #[must_use]
    pub fn store(&self) -> &OagwStore {
        &self.store
    }

    /// The management service every operation is issued against.
    #[must_use]
    pub fn service(&self) -> &ManagementService {
        &self.service
    }

    /// The permission enforcer, or `None` when the platform resolved no
    /// `AuthZ` client at startup.
    ///
    /// `None` fails closed: every management request is answered 403, because
    /// a permission this feature cannot check is a permission it must not
    /// grant.
    #[must_use]
    pub fn enforcer(&self) -> Option<&PolicyEnforcer> {
        self.enforcer.as_deref()
    }

    /// The cache the write path advances and the data plane observes.
    #[must_use]
    pub fn cache(&self) -> &ControlPlaneCache {
        &self.cache
    }

    /// Registers the rate-limit cleanup the deletion seam notifies.
    ///
    /// `cpt-cf-oagw-feature-rate-limiting` owns the observer; this feature only
    /// forwards the registration to the service that issues the notification.
    pub fn register_deletion_observer(&self, observer: Arc<dyn RateLimitCleanup>) {
        self.service.register_deletion_observer(observer);
    }

    /// Assembles the management surface the gear mounts.
    ///
    /// One store, one Control Plane cache and one management service are built
    /// over the compiled configuration; the enforcer is the platform's
    /// `AuthZ` client when the hub resolved one and `None` when it did not, so
    /// a gear that starts without an `AuthZ` client serves a surface that
    /// answers 403 to every request rather than one that answers 200 to any.
    /// The tenant-resolver client is the same kind of opportunistic
    /// resolution, and a gear that starts without one serves a surface whose
    /// every bind and every resolution fails closed, because no ancestor chain
    /// can be obtained.
    ///
    /// # Errors
    ///
    /// Returns the refusal of the configuration compilation, which a
    /// configuration that loaded and validated cannot produce.
    #[allow(clippy::result_large_err)]
    pub fn assemble(
        config: &OagwConfig,
        authz: Option<Arc<dyn AuthZResolverClient>>,
        resolver: Option<Arc<dyn TenantResolverClient>>,
        cred_store: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Result<Self, crate::domain::error::DomainError> {
        // @cpt-begin:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-resolve
        let enforcer = authz.map(|client| Arc::new(PolicyEnforcer::new(client)));
        let cache = Arc::new(ControlPlaneCache::new());
        // @cpt-end:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-resolve

        let store = Arc::new(OagwStore::new());
        let service = Arc::new(ManagementService::new(
            Arc::clone(&store),
            config,
            Arc::clone(&cache),
        )?);
        let dp_cache = crate::data_plane::DpCache::new();
        // The Data Plane flush registers with the cache the write path
        // advances, so one successful write invalidates in the same process
        // and before the write's response is produced.
        cache.register_dp_flush(Arc::new(dp_cache.clone()));
        let state = Self {
            config: Arc::new(*config),
            store,
            service,
            enforcer,
            resolver,
            cache,
            dp_cache,
            outbound: OutboundClient::new(),
            round_robin: crate::data_plane::RoundRobin::new(),
            registries: Self::registries_for(cred_store, config),
            rate_limits: crate::data_plane::SharedLimits::new(),
            observability: Arc::new(
                crate::data_plane::observability::Observability::new(),
            ),
        };
        // The rate-limit cleanup registers with the same deletion seam the
        // data-plane flush does, so one successful upstream or route deletion
        // drops its counters before the delete's response is produced.
        let observer = std::sync::Arc::new(crate::data_plane::RegistryCleanup::new(
            state.rate_limits.clone(),
        ));
        state.register_deletion_observer(observer);
        Ok(state)
    }

    /// The Data Plane L1 cache the proxy resolution reads and populates.
    #[must_use]
    pub fn dp_cache(&self) -> &crate::data_plane::DpCache {
        &self.dp_cache
    }

    /// The shared outbound client the proxy forward dial through.
    #[must_use]
    pub fn outbound(&self) -> &OutboundClient {
        &self.outbound
    }

    /// The per-upstream round-robin counters the endpoint selection advances.
    #[must_use]
    pub fn round_robin(&self) -> &crate::data_plane::RoundRobin {
        &self.round_robin
    }

    /// The platform tenant-resolver client the ancestor chain comes from.
    ///
    /// `None` answers a hub that resolved nothing, and every caller of it
    /// fails closed: no chain means no bind and no resolution.
    #[must_use]
    pub fn resolver(&self) -> Option<&Arc<dyn TenantResolverClient>> {
        self.resolver.as_ref()
    }

    /// The per-instance rate-limit registry the proxy check charges and the
    /// deletion observer drops prefixes from.
    #[must_use]
    pub fn rate_limits(&self) -> &crate::data_plane::SharedLimits {
        &self.rate_limits
    }

    /// The plugin registries the chain composition resolves built-ins through.
    #[must_use]
    pub fn registries(&self) -> &crate::plugins::PluginRegistries {
        &self.registries
    }

    /// The in-process observation seam the proxy path's entry and exit read
    /// and the metrics path renders from.
    #[must_use]
    pub fn observability(
        &self,
    ) -> &Arc<crate::data_plane::observability::Observability> {
        &self.observability
    }

    /// Replaces the audit sink the seam writes its records to.
    ///
    /// A deployment that leaves the stdout sink in place writes the stream to
    /// the stdout the single executable owns; a test installs a collecting
    /// sink, which is the mock boundary `cpt-cf-oagw-dod-obs-tests` names.
    pub fn swap_audit_sink(
        &self,
        sink: Arc<dyn crate::data_plane::observability::AuditSink>,
    ) {
        self.observability.swap_sink(sink);
    }
}
