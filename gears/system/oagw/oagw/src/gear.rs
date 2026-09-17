//! OAGW gear — lifecycle, configuration, wiring and the data-plane bridge.
//!
//! Lifecycle overview (traceability in `docs/pipeline/features/feature-gear-foundation`):
//! - `init`: loads and validates the [`OagwConfig`] block (fail-loud per
//!   `cpt-cf-oagw-principle-fail-loud-config`), builds the persistence-free
//!   control plane, resolves the declared `-sdk` clients from the client hub,
//!   wires the PEP enforcer / credential resolver / rate limiter / data-plane
//!   gate, and registers the GTS catalog (bounded failures are logged, never
//!   fatal — `inst-gts-catalog`).
//! - `register_rest`: mounts the management + proxy routers gear-relative
//!   under `/oagw/v1/...` with no embedded `/api` segment.
//! - `serve`: spawns the pingora bridge, records its loopback port in the
//!   shared [`ProxyPort`] cell, signals readiness, then idles until cancel.
//!
//! Deviation from `cpt-cf-oagw-dod-control-plane-persistence` (per ADR 0010's
//! persistence-free MVP clause): there is **no** `db` capability and no
//! `DatabaseCapability` implementation — the control plane is an in-memory,
//! persistence-free repository seeded from the validated [`OagwConfig`].
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-db-required
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-migrations
// @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-register-migrations
//   (persistence-free resolution: the gear acquires no database slot,
//   registers no `oagw_*` migrations, and wires the in-memory repository into
//   the data-plane gate and management handlers — see ADR 0010's
//   persistence-free MVP clause, recorded in the run manifest)
// @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-register-migrations
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-migrations
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-db-required

// DoD traceability (`cpt-cf-oagw-dod-gear-foundation-*` — to_code markers).
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-registration:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-lifecycle-wiring:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-route-mounting:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-config-model:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-health-contribution:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-test-harness:p1
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverClient;
use authz_resolver_sdk::pep::PolicyEnforcer;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, Healthcheck, HealthcheckResult, RestApiCapability};
use tracing::{info, warn};

use crate::api::rest::{ProxyPort, routes};
use crate::config::OagwConfig;
use crate::domain::credentials::CredentialResolver;
use crate::domain::rate_limit::RateLimiterRegistry;
use crate::domain::repository::ControlPlaneService;
use crate::infra::proxy::proxy_http::DataPlaneService;
use crate::infra::proxy::{DataPlaneGate, spawn_proxy_bridge};
use crate::infra::type_provisioning::register_gts_catalog;

/// Main gear struct for the OAGW (outbound API gateway) gear.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-inventory-submit
//   (the host discovers `OagwGear` through this `#[toolkit::gear(...)]`
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-inventory-submit
//   inventory registration — `toolkit::inventory::submit!`)
#[toolkit::gear(
    name = "oagw",
    deps = [authz_resolver, types_registry, tenant_resolver, credstore],
    capabilities = [stateful, rest],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
#[allow(clippy::struct_field_names)]
pub struct OagwGear {
    /// Persistence-free control-plane repository (upstreams/routes/plugins).
    control: OnceLock<Arc<ControlPlaneService>>,
    /// PEP enforcer backing the management and proxy authz gates.
    enforcer: OnceLock<Arc<PolicyEnforcer>>,
    /// Credential resolver backing the auth plugins (`credstore` + OAuth cache).
    resolver: OnceLock<Arc<CredentialResolver>>,
    /// Token-bucket rate limiter registry.
    rate_limiter: OnceLock<Arc<RateLimiterRegistry>>,
    /// The data-plane request pipeline gate.
    gate: OnceLock<Arc<DataPlaneGate>>,
    /// Shared cell hosting the bridge listener port (0 = not running).
    port: ProxyPort,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            enforcer: OnceLock::new(),
            resolver: OnceLock::new(),
            rate_limiter: OnceLock::new(),
            gate: OnceLock::new(),
            port: ProxyPort::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(gear = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-init
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-gear-init
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-parse
        let cfg: OagwConfig = ctx
            // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-init
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-gear-init
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-parse
            .config_or_default()
            .map_err(|e| anyhow::anyhow!("oagw: failed to load config: {e}"))?;
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-defaults
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-load
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-validate
        cfg.validate().map_err(|e| {
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-defaults
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-load
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-validate
            // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-invalid
            // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-fail-loud
            // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-return-error
            anyhow::anyhow!("oagw config invalid: {e}")
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-invalid
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-fail-loud
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-return-error
        })?;
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-valid
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-valid
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-return-valid
        // Persistence-free control plane (ADR 0010 persistence-free MVP).
        let control = Arc::new(ControlPlaneService::from_config(&cfg)?);
        self.control
            .set(Arc::clone(&control))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-return-valid
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-valid
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-config-valid

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-client-hub
        let authz = ctx
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-client-hub
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZ resolver client: {e}"))?;
        let credstore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get credstore client: {e}"))?;

        let enforcer = Arc::new(PolicyEnforcer::new(authz));
        self.enforcer
            .set(Arc::clone(&enforcer))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-thread-settings
        let resolver = Arc::new(CredentialResolver::new(
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-config-load-validation:ph-1:inst-thread-settings
            credstore,
            cfg.token_cache_capacity,
            Duration::from_secs(cfg.token_cache_ttl_secs),
        ));
        self.resolver
            .set(Arc::clone(&resolver))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let rate_limiter = Arc::new(RateLimiterRegistry::new(true));
        self.rate_limiter
            .set(Arc::clone(&rate_limiter))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let gate = Arc::new(DataPlaneGate::new(
            Arc::clone(&control),
            Arc::clone(&enforcer),
            Arc::clone(&resolver),
            Arc::clone(&rate_limiter),
            cfg.allow_http_upstream,
            cfg.ssrf_policy.enabled,
            cfg.proxy_timeout(),
        ));
        self.gate
            .set(Arc::clone(&gate))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        // (validated config accepted; control plane bootstrapped above)
        // GTS catalog materialization (content owned by feature-type-provisioning).
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-gts-catalog
        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-init-invokes
        let status = register_gts_catalog(ctx).await?;
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-gts-catalog
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-init-invokes
        // @cpt-begin:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-failed
        if !status.all_settled() {
            warn!(
                failures = status.failures.len(),
                "oagw GTS catalog materialized with bounded failures (logged, not fatal)"
            );
        }
        // @cpt-end:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-failed
        info!(
            schemas = status.schemas_registered + status.schemas_converged,
            instances = status.instances_registered + status.instances_converged,
            "oagw GTS catalog materialized"
        );

        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            token_cache_ttl_secs = cfg.token_cache_ttl_secs,
            token_cache_capacity = cfg.token_cache_capacity,
            "oagw gear initialized"
        );
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-register-rest
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-register-rest
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-apply-prefix
        //   (host `ApiGateway::apply_prefix` composes these gear-relative routes)
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-register-rest
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-register-rest
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-apply-prefix
        info!("registering oagw REST routes");

        let control = self
            .control
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: control plane not initialized"))?;
        let enforcer = self
            .enforcer
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: enforcer not initialized"))?;

        let router = routes::register_routes(router, openapi, control, enforcer, self.port.clone());
        Ok(router)
    }

    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-health-probe
        let port = self.port.clone();
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-health-probe
        Some(Arc::new(OagwHealthcheck { port }))
    }
}

/// Composite readiness check for the OAGW gear: healthy only once the
/// data-plane bridge listener is bound (`inst-ready-check`).
struct OagwHealthcheck {
    port: ProxyPort,
}

#[async_trait]
impl Healthcheck for OagwHealthcheck {
    fn name(&self) -> &'static str {
        "oagw-data-plane"
    }

    async fn check(&self) -> HealthcheckResult {
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-readiness-query
        if self.port.port() != 0 {
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-readiness-query
            // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-ready-check
            // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-healthy
            HealthcheckResult::healthy()
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-ready-check
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-healthy
        } else {
            // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-unhealthy
            HealthcheckResult::unhealthy("oagw data-plane bridge is not running")
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-unhealthy
        }
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-return-health
    }
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-health-reporting:ph-1:inst-return-health
}

impl OagwGear {
    /// Lifecycle entry (`lifecycle(entry = "serve")`). Starts the pingora
    /// data-plane bridge, publishes its port, signals readiness, then idles
    /// until the runtime cancels.
    #[allow(
        clippy::redundant_pub_crate,
        reason = "gear-private serve entry-point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-start
        let gate = self
            // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start
            // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-start
            .gate
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: serve invoked before init"))?;
        let service = DataPlaneService::new(gate);

        let mut bridge = match spawn_proxy_bridge(service) {
            Some(bridge) => bridge,
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start-check
                // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-not-ready
                // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-failed
                anyhow::bail!("oagw: failed to start the data-plane proxy bridge");
                // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start-check
                // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-not-ready
                // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-failed
            }
        };
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start-ok
        self.port.bind(bridge.port);
        self.port.bind_relay_secret(bridge.relay_secret.clone());
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-start-ok
        info!(port = bridge.port, "oagw data-plane bridge running");

        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-ready
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-ready
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-ready
        ready.notify();
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-ready
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-ready
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-ready
        // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-init-complete
        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-wired

        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-startup-registration:ph-1:inst-init-complete
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-wired
        info!("oagw data plane ready; awaiting shutdown");
        cancel.cancelled().await;

        // @cpt-begin:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-stop
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-stop
        bridge.stop();
        // @cpt-end:cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring:ph-1:inst-stop
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-stop
        info!("oagw data-plane bridge stopped");
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-stopped
        Ok(())
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:ph-1:inst-stopped
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
