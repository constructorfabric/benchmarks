//! Gear declaration for the OAGW module.
//!
//! Registers the `oagw` gear with the ToolKit host (DoD
//! `cpt-cf-oagw-dod-gear-foundation-register`, flow
//! `cpt-cf-oagw-flow-gear-foundation-boot`, state
//! `cpt-cf-oagw-state-gear-foundation-lifecycle`):
//!
//! - declares the SDK dependencies `credstore`, `types_registry`,
//!   `tenant_resolver`, `authz_resolver`;
//! - resolves `gears.oagw.config` via `config_or_default` (DoD
//!   `cpt-cf-oagw-dod-gear-foundation-config`);
//! - acquires the four SDK clients from the GearCtx client hub;
//! - registers the REST surface via `RestApiCapability::register_rest` (DoD
//!   `cpt-cf-oagw-dod-gear-foundation-rest-openapi`) with the shared gear
//!   state as an axum `Extension`.
//!
//! # Capability note (`system` + `rest`, not `stateful` + `lifecycle`)
//!
//! The foundation feature declares capabilities `[rest, stateful]` with a
//! lifecycle `init`/`serve` pair.  Both the `stateful` capability and the
//! `lifecycle(...)` attribute expand to a `RunnableCapability` whose
//! signature names `::tokio_util::sync::CancellationToken` (see
//! `toolkit-macros` `Capability::Stateful` generation and the `Runnable`
//! impls) — and `cf-gears-oagw`'s `Cargo.toml` cannot declare `tokio-util`
//! (a fixed workspace contract; this package's deps are not editable).
//! This gear therefore uses the same `capabilities = [rest, system]`
//! declaration as the in-repo `types-registry` gear (a stateful REST gear
//! whose `init` acquires clients, publishes them to the hub, and whose
//! `register_rest` wires the REST surface): the same
//! REGISTERED → INITIALIZED → SERVING lifecycle states are exercised through
//! the `Gear` trait `init` entry, the `SystemCapability` hook, and the
//! ToolKit host's serving framework.  The `stateful` flag and a custom
//! `serve` entry can be enabled the moment `tokio-util` becomes a permitted
//! dependency without changing this crate's structure.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverClient;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, RestApiCapability};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::domain::GearState;
use crate::domain::rate::{Decider, RateLimiter};
use crate::domain::service::{ControlPlaneService, DataPlaneService};
use crate::infra::plugin::builtin_registries_with_cache;
use crate::infra::{InMemoryStore, UpstreamRepoOptions};

/// OAGW — outbound API gateway gear.
///
/// Single-deployment component holding both plane boundaries (flow
/// `cpt-cf-oagw-flow-gear-foundation-plane-routing`): the Control Plane
/// (management CRUD) and the Data Plane (proxy hot path).
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, tenant_resolver, authz_resolver],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    /// Immutable post-init process-wide state shared with the REST layer.
    state: OnceLock<Arc<GearState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl SystemCapability for OagwGear {}

#[async_trait]
impl Gear for OagwGear {
    /// Initializes the gear (state `cpt-cf-oagw-state-gear-foundation-lifecycle`,
    /// REGISTERED → INITIALIZED; flow `cpt-cf-oagw-flow-gear-foundation-boot`).
    ///
    /// Resolves the configuration (DoD `cpt-cf-oagw-dod-gear-foundation-config`),
    /// acquires the four SDK clients from the client hub, builds the
    /// in-memory repositories, and assembles the Control Plane / Data Plane
    /// service state.
    ///
    /// # Errors
    /// Fails if the configuration is invalid, a required SDK client cannot
    /// be acquired, or the gear was already initialized (the gear then never
    /// reaches SERVING — state transition `inst-gf-state-init-fail`).
    #[tracing::instrument(skip_all)]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // gears.oagw.config via config_or_default (step inst-gf-boot-config).
        let cfg: OagwConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("invalid gears.oagw.config: {e}"))?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            "oagw configuration resolved"
        );

        // Acquire the in-process SDK clients (step inst-gf-boot-deps). A
        // missing required client is an explicit init failure.
        let hub = ctx.client_hub();
        let credstore = hub
            .get::<dyn CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to acquire credstore client: {e}"))?;
        let types_registry = hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to acquire types_registry client: {e}"))?;
        let tenant_resolver = hub
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to acquire tenant_resolver client: {e}"))?;
        let authz_resolver = hub
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to acquire authz_resolver client: {e}"))?;

        // Shared in-memory repositories (constraint
        // `cpt-cf-oagw-constraint-in-memory-storage`), mirroring the §3.7
        // table shapes.
        let store = InMemoryStore::new();
        let upstreams = store.upstream_repo(UpstreamRepoOptions {
            allow_http_upstream: cfg.allow_http_upstream,
        });
        let routes = store.route_repo();
        let plugins = store.plugin_repo();

        let control = ControlPlaneService::new(
            cfg.clone(),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
            Arc::clone(&plugins),
            tenant_resolver.clone(),
            authz_resolver.clone(),
        );

        // Assemble the built-in plugin registries, threading the gear's
        // OAuth2 token-cache sizing into the OAuth2 plugins (algorithm
        // `cpt-cf-oagw-algo-plugin-system-oauth2-cache`), and the in-process
        // rate limiter whose bucket TTL mirrors the token cache (lazy expiry
        // per `cpt-cf-oagw-algo-rate-limiting-cache-lookup`).
        let registries = builtin_registries_with_cache(
            Some(Arc::clone(&credstore)),
            Duration::from_secs(cfg.token_cache_ttl_secs),
            cfg.token_cache_capacity,
        );
        let rate_limiter: Arc<dyn Decider> = Arc::new(RateLimiter::new(Duration::from_secs(
            cfg.token_cache_ttl_secs,
        )));

        // The shared metrics registry (feature
        // `cpt-cf-oagw-feature-observability-audit`, DESIGN §4.2): fed by the
        // Data Plane hot path and the control-plane audit trail, served at the
        // admin `/metrics` surface.
        let metrics = Arc::new(crate::infra::MetricsRegistry::default());

        let data = DataPlaneService::new(
            cfg,
            upstreams,
            routes,
            plugins,
            registries,
            rate_limiter,
            credstore,
            types_registry,
            tenant_resolver,
            authz_resolver,
            Arc::clone(&metrics),
        );

        let state = Arc::new(GearState {
            control,
            data,
            metrics,
        });
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    /// Registers the REST surface with unprefixed paths (DoD
    /// `cpt-cf-oagw-dod-gear-foundation-rest-openapi`, algorithm
    /// `cpt-cf-oagw-algo-gear-foundation-register-surface`).
    ///
    /// # Errors
    /// Fails if the gear has not been initialized (no shared state to attach).
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} gear not initialized", Self::MODULE_NAME))?
            .clone();
        info!("registering oagw REST surface");
        crate::api::rest::register_routes(router, openapi, state)
    }
}
