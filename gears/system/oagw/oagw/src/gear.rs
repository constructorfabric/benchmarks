//! The OAGW gear (`cpt-cf-oagw-flow-gear-foundation-gear-init`).
//!
//! Declared with `#[toolkit::gear]` as `oagw`, depending on `types_registry`,
//! `authz_resolver`, `tenant_resolver` and `credstore`, and contributing to
//! the api-gateway's router through `rest`. Every field is a
//! `OnceLock`/`Mutex<Option<_>>` so the runtime can construct the gear before
//! `init` runs.
//!
//! # The init sequence (`inst-gf-init-1` .. `-10`)
//!
//! 1. the ToolKit runtime instantiates the gear and invokes `Gear::init`
//!    (`inst-gf-init-1`);
//! 2. `OagwConfig` is resolved through the toolkit configuration lookup with
//!    the declared defaults for absent keys (`inst-gf-init-2`);
//! 3. **IF** parsing or validation fails, initialization aborts with the
//!    validation error and no REST route is registered
//!    (`inst-gf-init-3`/`-4`);
//! 4. the in-memory repositories, the services that consume them and the four
//!    external dependency clients are resolved (`inst-gf-init-5`);
//! 5. the base GTS types register through `types_registry`
//!    (`inst-gf-init-6`);
//! 6. **IF** a registration is rejected or the registry is unreachable,
//!    initialization aborts with the provisioning error
//!    (`inst-gf-init-7`/`-8`);
//! 7. the service handles are published to the client hub and the gear marks
//!    itself ready (`inst-gf-init-9`);
//! 8. the returned gear is entirely in-process and contributes its REST
//!    surface through `RestApiCapability` (`inst-gf-init-10`).
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1
// @cpt-flow:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverClient;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::Gear;
use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use toolkit::contracts::RestApiCapability;
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::domain::services::ControlPlaneService;
use crate::domain::services::management::UpstreamManagementService;
use crate::domain::services::route_management::RouteManagementService;
use crate::infra::authorization::{AuthzManagementAuthorizer, TenantHierarchyAncestors};
use crate::infra::storage::Storage;

// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-10
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-8
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9c
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-10
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-8
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-9
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-8
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5c
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5d
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-6
/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, authz_resolver, tenant_resolver, credstore],
    capabilities = [rest],
)]
pub struct OagwGear {
    /// The parsed and validated configuration block.
    pub(crate) config: OnceLock<OagwConfig>,
    /// The in-memory repositories and the service that consumes them.
    pub(crate) storage: OnceLock<Arc<Storage>>,
    /// The control-plane service handle published to the client hub.
    pub(crate) service: OnceLock<Arc<dyn ControlPlaneService>>,
    /// The upstream-management aggregate of entry 2.2, built at init.
    pub(crate) management: OnceLock<Arc<UpstreamManagementService>>,
    /// The route-management aggregate of entry 2.3, built at init.
    pub(crate) routes: OnceLock<Arc<RouteManagementService>>,
    /// The proxy data-plane pipeline of entry 2.4, built at init.
    pub(crate) data_plane: OnceLock<Arc<crate::infra::proxy::DataPlaneServiceImpl>>,
    /// The plugin-catalog aggregate of entry 2.6, built at init.
    pub(crate) plugin_management: OnceLock<Arc<crate::domain::services::plugin_management::PluginManagementService>>,
    /// The plugin runtime of entry 2.6: the registries and the chain executor.
    pub(crate) plugin_runtime: OnceLock<Arc<crate::infra::plugin::executor::PluginRuntime>>,
    /// The Control Plane L1 cache of entry 2.9, built at init.
    pub(crate) cp_state: OnceLock<crate::infra::cp_cache::CPState>,
    /// The Data Plane L1 hot-configuration cache of entry 2.9, built at init.
    pub(crate) hot_config: OnceLock<Arc<crate::infra::dp_cache::DpHotConfig>>,
    /// The metric registry of entry 2.9, built at init.
    pub(crate) metrics: OnceLock<Arc<crate::infra::metrics::MetricsRegistry>>,
    /// The audit emitter of entry 2.9, built at init.
    pub(crate) audit: OnceLock<Arc<crate::infra::audit::AuditSink>>,
    /// The observability seam the proxy surface records through.
    pub(crate) observability: OnceLock<Arc<crate::infra::observability::Observability>>,
    /// The health state the readiness hook reports.
    pub(crate) health: OnceLock<Arc<crate::infra::health::HealthStateHolder>>,
    /// Resolved dependency handles; first invoked by later entries.
    pub(crate) types_registry: Mutex<Option<Arc<dyn TypesRegistryClient>>>,
    pub(crate) authz_resolver: Mutex<Option<Arc<dyn AuthZResolverClient>>>,
    pub(crate) tenant_resolver: Mutex<Option<Arc<dyn TenantResolverClient>>>,
    pub(crate) credstore: Mutex<Option<Arc<dyn CredStoreClientV1>>>,
    /// The audit sink a test installed before `init`, so the emitted lines are
    /// assertable instead of written to stdout.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) audit_override: Mutex<Option<Arc<crate::infra::audit::AuditSink>>>,
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-breaker-metric-surface:p1:inst-os-cb-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9c
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-9
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-8
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-10
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-read:p1:inst-os-cpread-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-flush:p1:inst-os-dpflush-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-9
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-8
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-10
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-dp-cache-read:p1:inst-os-dpread-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-8
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-rate-limit-signals:p1:inst-os-rl-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5d
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5c
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-shared-http-client:p1:inst-os-client-1
//

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            storage: OnceLock::new(),
            service: OnceLock::new(),
            management: OnceLock::new(),
            routes: OnceLock::new(),
            data_plane: OnceLock::new(),
            plugin_management: OnceLock::new(),
            plugin_runtime: OnceLock::new(),
            cp_state: OnceLock::new(),
            hot_config: OnceLock::new(),
            metrics: OnceLock::new(),
            audit: OnceLock::new(),
            observability: OnceLock::new(),
            health: OnceLock::new(),
            types_registry: Mutex::new(None),
            authz_resolver: Mutex::new(None),
            tenant_resolver: Mutex::new(None),
            credstore: Mutex::new(None),
            #[cfg(any(test, feature = "test-utils"))]
            audit_override: Mutex::new(None),
        }
    }
}

impl OagwGear {
    /// The validated configuration, available after `init`.
    #[must_use]
    pub fn config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// The storage handle, available after `init`.
    #[must_use]
    pub fn storage(&self) -> Option<Arc<Storage>> {
        self.storage.get().cloned()
    }

    /// The published control-plane service, available after `init`.
    #[must_use]
    pub fn service(&self) -> Option<Arc<dyn ControlPlaneService>> {
        self.service.get().cloned()
    }

    /// The upstream-management aggregate, available after `init`.
    #[must_use]
    pub fn management(&self) -> Option<Arc<UpstreamManagementService>> {
        self.management.get().cloned()
    }

    /// The route-management aggregate, available after `init`.
    #[must_use]
    pub fn routes(&self) -> Option<Arc<RouteManagementService>> {
        self.routes.get().cloned()
    }

    /// The proxy data-plane pipeline, available after `init`.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<crate::infra::proxy::DataPlaneServiceImpl>> {
        self.data_plane.get().cloned()
    }

    /// The plugin-catalog aggregate, available after `init`.
    #[must_use]
    pub fn plugin_management(
        &self,
    ) -> Option<Arc<crate::domain::services::plugin_management::PluginManagementService>> {
        self.plugin_management.get().cloned()
    }

    /// The plugin runtime, available after `init`.
    #[must_use]
    pub fn plugin_runtime(
        &self,
    ) -> Option<Arc<crate::infra::plugin::executor::PluginRuntime>> {
        self.plugin_runtime.get().cloned()
    }

    /// The Control Plane L1 cache, available after `init`.
    #[must_use]
    pub fn cp_state(&self) -> Option<crate::infra::cp_cache::CPState> {
        self.cp_state.get().cloned()
    }

    /// The Data Plane L1 hot-configuration cache, available after `init`.
    #[must_use]
    pub fn hot_config(&self) -> Option<Arc<crate::infra::dp_cache::DpHotConfig>> {
        self.hot_config.get().cloned()
    }

    /// The metric registry, available after `init`.
    #[must_use]
    pub fn metrics(&self) -> Option<Arc<crate::infra::metrics::MetricsRegistry>> {
        self.metrics.get().cloned()
    }

    /// The audit emitter, available after `init`.
    #[must_use]
    pub fn audit(&self) -> Option<Arc<crate::infra::audit::AuditSink>> {
        self.audit.get().cloned()
    }

    /// The observability seam the proxy surface records through, available
    /// after `init`.
    #[must_use]
    pub fn observability(&self) -> Option<Arc<crate::infra::observability::Observability>> {
        self.observability.get().cloned()
    }

    /// The health state the readiness hook reports, available after `init`.
    #[must_use]
    pub fn health(&self) -> Option<Arc<crate::infra::health::HealthStateHolder>> {
        self.health.get().cloned()
    }

    /// Install the audit sink `init` wires the observability seam and the
    /// configuration-write hook to, instead of the stdout emitter.
    ///
    /// Test-only: the graded deployment always writes stdout, and the lines a
    /// test asserts are otherwise not observable.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn install_audit_sink(&self, sink: Arc<crate::infra::audit::AuditSink>) {
        *self.audit_override.lock().expect("audit override lock") = Some(sink);
    }
}

// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-1
// The ToolKit runtime instantiates the gear declared with `#[toolkit::gear]`
// and invokes `Gear::init` with the gear context.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-3
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-4
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-7
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-8
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-1
#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Entry 2.9: the health machine is wired **first**, so a gear that
        // fails at any later step — a rejected configuration block included —
        // is still reportable, from the `initializing` state it stopped in and
        // never as ready
        // (`cpt-cf-oagw-dod-observability-and-state-health-readiness`).
        let health = Arc::new(crate::infra::health::HealthStateHolder::default());
        health.initializing();
        let _ = self.health.set(Arc::clone(&health));

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-2
        // Resolve `OagwConfig` through the toolkit configuration lookup,
        // applying the declared defaults for absent keys.
        let config: OagwConfig = ctx.config_or_default()?;
        // inst-gf-init-3/-4: fail fast on an invalid block, before any
        // repository, service or route exists.
        config
            .validate()
            .map_err(|error| anyhow::anyhow!("oagw config invalid: {error}"))?;
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-2
        info!(
            allow_http_upstream = config.allow_http_upstream,
            proxy_timeout_secs = config.proxy_timeout_secs,
            "initializing oagw module"
        );

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-5
        // inst-gf-init-5: the in-memory, config-backed repositories and the
        // services that consume them.
        let storage = Arc::new(Storage::new());
        let (raw_upstreams, routes, plugins) = storage.repositories();
        let route_store = Arc::clone(&routes);
        let plugin_store = Arc::clone(&plugins);

        // @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-1
        // Entry 2.9: the state surface is constructed before any service
        // consumes it, and the health machine is already in `initializing`
        // from the first step of the sequence.
        let cp_state = crate::infra::cp_cache::CPState::single_executable();
        let hot_config = Arc::new(crate::infra::dp_cache::DpHotConfig::new(storage.generations()));
        let metrics = Arc::new(crate::infra::metrics::MetricsRegistry::new());
        let audit = {
            // A test may have installed the sink it asserts against; the graded
            // deployment always writes stdout
            // (`cpt-cf-oagw-dod-observability-and-state-audit-log`).
            #[cfg(any(test, feature = "test-utils"))]
            let sink = self
                .audit_override
                .lock()
                .expect("audit override lock")
                .take()
                .unwrap_or_else(crate::infra::audit::AuditSink::to_stdout);
            #[cfg(not(any(test, feature = "test-utils")))]
            let sink = crate::infra::audit::AuditSink::to_stdout();
            sink
        };
        let observability = Arc::new(crate::infra::observability::Observability::new(
            Arc::clone(&metrics),
            Arc::clone(&audit),
        ));
        // The Control Plane L1 cache is the caching decorator implementing the
        // domain repository trait, so every consumer of the upstream store —
        // the control-plane service, the three management aggregates and the
        // proxy data plane — reads through it without knowing the cache exists
        // (`cpt-cf-oagw-dod-observability-and-state-cp-cache`).
        let upstreams: Arc<dyn crate::domain::repo::UpstreamRepository> = Arc::new(
            crate::infra::cp_cache::CachedUpstreamRepository::new(
                raw_upstreams,
                cp_state.clone(),
                storage.generations(),
            ),
        );
        // @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-1

        let service: Arc<dyn ControlPlaneService> = Arc::new(
            crate::domain::services::ControlPlaneServiceImpl::new(
                Arc::clone(&upstreams),
                routes,
                plugins,
                config.allow_http_upstream,
            ),
        );

        // inst-gf-init-5: the external dependency clients, resolved as
        // handles only and first invoked by later entries.
        let types_registry = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|error| anyhow::anyhow!("failed to get TypesRegistryClient: {error}"))?;
        let authz_resolver = ctx
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|error| anyhow::anyhow!("failed to get AuthZResolverClient: {error}"))?;
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|error| anyhow::anyhow!("failed to get TenantResolverClient: {error}"))?;
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|error| anyhow::anyhow!("failed to get CredStoreClientV1: {error}"))?;
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-5

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-5
        // The management aggregate over the same store: permission-gated
        // through `authz_resolver`, tenant-hierarchy aware through
        // `tenant_resolver`.
        let management = Arc::new(UpstreamManagementService::new(
            Arc::clone(&upstreams),
            config.allow_http_upstream,
            Arc::new(AuthzManagementAuthorizer::new(Arc::clone(&authz_resolver)))
                as Arc<dyn crate::domain::services::management::ManagementAuthorizer>,
            Arc::new(TenantHierarchyAncestors::new(Arc::clone(&tenant_resolver)))
                as Arc<dyn crate::domain::services::management::AncestorResolver>,
        ));

        // Entry 2.3: the route-management aggregate over the same store, gated
        // by the same authorizer and resolving the plugin bindings of a route
        // write through the plugin-catalog boundary the foundation entry
        // provisioned.
        let routes_service = Arc::new(RouteManagementService::new(
            Arc::clone(&route_store),
            Arc::clone(&upstreams),
            Arc::new(crate::infra::plugin::CatalogBindingResolver::new(Arc::clone(&plugin_store))),
            Arc::new(AuthzManagementAuthorizer::new(Arc::clone(&authz_resolver)))
                as Arc<dyn crate::domain::services::management::ManagementAuthorizer>,
        ));

        // Entry 2.4: the proxy data plane over the same stores, gated by the
        // same authorizer and tenant-hierarchy aware through the same
        // resolver, bounded by the configuration limits.
        let data_plane = crate::infra::proxy::DataPlaneServiceImpl::new(
            Arc::clone(&upstreams),
            Arc::clone(&route_store),
            Arc::new(TenantHierarchyAncestors::new(Arc::clone(&tenant_resolver)))
                as Arc<dyn crate::domain::services::management::AncestorResolver>,
            Arc::new(AuthzManagementAuthorizer::new(Arc::clone(&authz_resolver)))
                as Arc<dyn crate::domain::services::management::ManagementAuthorizer>,
            crate::infra::proxy::DataPlaneLimits::of(&config),
        );

        // Entry 2.6: the three plugin registries over the built-in plugins,
        // the credential store they resolve `cred://` references from at
        // request time, the in-process token cache of the configuration block
        // and the plugin rows the custom-plugin management surface writes; the
        // chain executor the data plane runs every composed chain through.
        let registries = Arc::new(crate::infra::plugin::resolution::PluginRegistries::with_builtins(
            Arc::clone(&credstore),
            config.token_cache_config(),
            Arc::clone(&plugin_store),
        ));
        let runtime = Arc::new(crate::infra::plugin::executor::PluginRuntime::new(
            registries,
            config.proxy_timeout_secs,
        ));
        // The availability gauge follows the connection outcome, so the data
        // plane reports every selected endpoint's reachability to the
        // observability seam at the transport boundary
        // (`cpt-cf-oagw-flow-observability-and-state-request-metrics`).
        let transport_observer: Arc<dyn crate::infra::proxy::TransportObserver> = observability.clone();
        let data_plane = Arc::new(
            data_plane
                .with_plugin_runtime(Arc::clone(&runtime))
                .with_hot_config(Arc::clone(&hot_config))
                .with_transport_observer(transport_observer),
        );

        // Entry 2.6: the plugin-catalog aggregate over the same store, behind
        // the same authorization seam as the other two management aggregates.
        // It also owns the binding-time `auth.config` validation the upstream
        // write path executes (`inst-ps-bind-14`/`-15`).
        let plugin_management = Arc::new(
            crate::domain::services::plugin_management::PluginManagementService::new(
                Arc::clone(&plugin_store),
                Arc::clone(&upstreams),
                Arc::clone(&route_store),
                Arc::new(AuthzManagementAuthorizer::new(Arc::clone(&authz_resolver)))
                    as Arc<dyn crate::domain::services::management::ManagementAuthorizer>,
            ),
        );
        management.set_plugin_config_validator(Arc::new(
            crate::infra::plugin::CatalogBindingResolver::new(Arc::clone(&plugin_store)),
        ));
        // Entry 2.6: the upstream write path resolves `plugins.items[]` through
        // the same plugin-catalog boundary, so a catalog-only identifier and a
        // reference the calling tenant does not hold are rejected before any
        // binding row is stored (`inst-ps-bind-2` .. `-8`).
        management.set_plugin_binding_resolver(Arc::new(
            crate::infra::plugin::CatalogBindingResolver::new(Arc::clone(&plugin_store)),
        ));

        // @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-6
        // Entry 2.9: the post-write invalidation and audit hook 2.2/2.3/2.6
        // left the seam for is installed on all three management aggregates, so
        // every accepted configuration write invalidates the Control Plane L1
        // entries, flushes the Data Plane L1 entries that depend on them and
        // emits the `config_change` record
        // (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`,
        // `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`).
        let hook: Arc<dyn crate::domain::services::management::ConfigWriteHook> = Arc::new(
            crate::infra::observability::ObservabilityHook::new(
                cp_state.clone(),
                Arc::clone(&hot_config),
                Arc::clone(&audit),
            ),
        );
        management.set_config_write_hook(Arc::clone(&hook));
        routes_service.set_config_write_hook(Arc::clone(&hook));
        plugin_management.set_config_write_hook(Arc::clone(&hook));
        // @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-6
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-5

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-6
        // inst-gf-init-6 + inst-gf-gts-1/-2: register the base GTS types
        // idempotently; a rejected or unreachable registry aborts
        // initialization with the provisioning error (`inst-gf-init-7`/`-8`).
        crate::infra::type_provisioning::register_base_types(types_registry.as_ref())
            .await
            .map_err(|error| anyhow::anyhow!("oagw base type provisioning failed: {error}"))?;
        // Entry 2.6: the catalog-only plugin identifiers are registered as GTS
        // instances so a reference to them resolves as a reference — and is
        // then rejected at binding time as unresolvable (graded deviation 10).
        crate::domain::type_catalog::register_plugin_catalog(types_registry.as_ref())
            .await
            .map_err(|error| anyhow::anyhow!("oagw plugin catalog provisioning failed: {error}"))?;
        info!("oagw plugin catalog provisioned");
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-6

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-9
        // inst-gf-init-9: publish the handles and mark the gear ready. Every
        // `set` is checked, so a second `init` on the same gear instance is a
        // rejected no-op rather than a silent overwrite.
        self.storage
            .set(Arc::clone(&storage))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.management
            .set(management)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.routes
            .set(routes_service)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.plugin_management
            .set(plugin_management)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.plugin_runtime
            .set(runtime)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.cp_state
            .set(cp_state)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.hot_config
            .set(Arc::clone(&hot_config))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.metrics
            .set(Arc::clone(&metrics))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.audit
            .set(Arc::clone(&audit))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.observability
            .set(Arc::clone(&observability))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        *self.types_registry.lock().expect("types_registry lock") = Some(Arc::clone(&types_registry));
        *self.authz_resolver.lock().expect("authz lock") = Some(Arc::clone(&authz_resolver));
        *self.tenant_resolver.lock().expect("tenant lock") = Some(Arc::clone(&tenant_resolver));
        *self.credstore.lock().expect("credstore lock") = Some(Arc::clone(&credstore));

        // @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-3
        // `inst-os-health-3`: every state component is wired — the CP L1 cache,
        // the DP L1 cache, the metrics registry and the audit emitter — so the
        // gear reports itself ready and the readiness hook serves `healthy`
        // (`inst-os-deploy-1`).
        health.ready();
        // @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-3

        info!("oagw module initialized");
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-9
    }
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-10
    // `inst-gf-init-10`: the returned gear is entirely in-process and
    // contributes its REST surface through `RestApiCapability`.
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-10
}
//
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-8
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-7
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-4
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-gear-init:p1:inst-gf-init-3
//

// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-4b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-2b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-4b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-1
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6c
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-7
impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-1
        // inst-gf-rest-1/-5: entry 2.1 declares the gear-relative tree and
        // registers no handler; later entries add the operations.
        let service = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} service not initialized", Self::MODULE_NAME))?;
        let router = crate::api::rest::register_rest(ctx, router, openapi, &service)?;
        // Entry 2.9: `GET /metrics`, registered first so the extension layers
        // it carries cover this route only.
        let metrics = self
            .metrics
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} metrics registry not initialized", Self::MODULE_NAME))?;
        let authz = self
            .authz_resolver
            .lock()
            .expect("authz lock")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{} authz resolver not initialized", Self::MODULE_NAME))?;
        let router = crate::api::rest::register_metrics_surface(
            router,
            openapi,
            metrics,
            Arc::new(crate::infra::authorization::MetricsGate::new(authz)),
        );
        // Entry 2.2: the five upstream operations of `/oagw/v1/upstreams`.
        let management = self
            .management
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} management service not initialized", Self::MODULE_NAME))?;
        let router = crate::api::rest::register_upstream_management(router, openapi, management);
        // Entry 2.3: the five route operations of `/oagw/v1/routes`.
        let routes = self
            .routes
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} route service not initialized", Self::MODULE_NAME))?;
        let router = crate::api::rest::register_route_management(router, openapi, routes);
        // Entry 2.4: the proxy catch-all of `/oagw/v1/proxy/{alias}`, with the
        // observability surface the handler records the exchange through.
        let data_plane = self
            .data_plane
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} data plane not initialized", Self::MODULE_NAME))?;
        let observability = self
            .observability
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} observability not initialized", Self::MODULE_NAME))?;
        let router = crate::api::rest::register_proxy_data_plane(
            router,
            openapi,
            data_plane,
            observability,
        );
        // Entry 2.6: the five plugin operations of `/oagw/v1/plugins`.
        let plugins = self
            .plugin_management
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} plugin service not initialized", Self::MODULE_NAME))?;
        Ok(crate::api::rest::register_plugin_management(router, openapi, plugins))
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-1
    }

    /// The readiness hook of entry 2.9, served by the aggregate `/readyz` and
    /// `/health` reports the framework renders
    /// (`cpt-cf-oagw-flow-observability-and-state-health-readiness`).
    fn healthcheck(
        &self,
        _ctx: &GearCtx,
    ) -> Option<std::sync::Arc<dyn toolkit::Healthcheck>> {
        // @cpt-begin:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-2
        // `inst-os-health-2`: one composite check per gear, over the health
        // state machine the init sequence drove to `ready`.
        let state = self.health.get().cloned()?;
        Some(Arc::new(crate::infra::health::GearHealth::new(state)))
        // @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-2
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-4b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation:p1:inst-os-cpinv-1
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-4b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-2b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-deployment-mode:p1:inst-os-deploy-2
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6c
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-health-readiness:p1:inst-os-health-1
//
