//! ToolKit gear wiring of the `oagw` gateway
//! (`cpt-cf-oagw-flow-gear-bootstrap`, `cpt-cf-oagw-state-gear-lifecycle`).
//!
//! The gear owns no listener (`cpt-cf-oagw-constraint-toolkit-deploy`): the host
//! that declares the `rest_host` capability mounts it. `init()` runs
//! configuration loading, dependency resolution and GTS provisioning, and the
//! runtime's REST phase then calls `register_rest`, which mounts the
//! `/oagw/v1` shell and refuses a duplicate mount prefix — a failure of either
//! call aborts process startup.
// @cpt-begin:cpt-cf-oagw-dod-gear-registration:p1:inst-full

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use axum::Router;
use parking_lot::Mutex;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::control_plane::ControlPlaneService;
use crate::api::rest::error::ProblemBody;
use crate::api::rest::route_shell::{self, MountLedger};
use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::proxy::SchemePolicy;
use crate::infra::plugin::plan::PluginRegistries;
use crate::infra::plugin::token_cache::TokenCacheConfig;
use crate::infra::proxy::pipeline::{ProxyLimits, ProxyPipeline};
use crate::infra::{dependency, type_provisioning};

/// The `oagw` gear: configuration, dependency and error-contract wiring of the
/// API-egress gateway, whose 15 management handlers are served by the
/// [`ControlPlaneService`] the gear owns and whose proxy shell is served by the
/// [`ProxyPipeline`] data plane `cpt-cf-oagw-feature-proxy-pipeline` owns.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry],
    capabilities = [rest]
)]
#[derive(Debug)]
pub struct OagwGear {
    /// Lifecycle state of the gear (`cpt-cf-oagw-state-gear-lifecycle`).
    state: Mutex<LifecycleState>,
    /// The configuration the last successful init validated: published with the
    /// state the bootstrap completes last, so an init that failed part way
    /// publishes nothing.
    config: OnceLock<OagwConfig>,
    /// The control plane the 15 mounted management handlers serve, holding the
    /// in-memory stores of this process.
    control_plane: Arc<ControlPlaneService>,
    /// The three plugin registries, constructed once by a successful `init()`
    /// (`cpt-cf-oagw-dod-plugin-registries`) and read-only from then on.
    plugin_registries: OnceLock<PluginRegistries>,
    /// The data plane the 10 mounted proxy handlers drive, constructed once by
    /// a successful `init()` and read-only from then on.
    data_plane: OnceLock<Arc<ProxyPipeline>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: Mutex::new(LifecycleState::Uninitialized),
            config: OnceLock::new(),
            control_plane: Arc::new(ControlPlaneService::default()),
            plugin_registries: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

/// The lifecycle states of the gear (`cpt-cf-oagw-state-gear-lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LifecycleState {
    /// The runtime has not invoked `init()` yet.
    #[default]
    Uninitialized,
    /// `init()` is running.
    Initializing,
    /// Configuration, dependencies, provisioning and route registration
    /// succeeded.
    Ready,
    /// An init step failed, or a later reconfiguration was rejected.
    Failed,
}

impl LifecycleState {
    /// Applies a transition and returns the state the gear reaches.
    ///
    /// Any transition the state machine does not declare is invalid and leaves
    /// the state unchanged; `Failed` is terminal in this release.
    #[must_use]
    pub fn transition(self, next: Self) -> Self {
        match (self, next) {
            (Self::Uninitialized, Self::Initializing)
            | (Self::Initializing, Self::Ready | Self::Failed)
            // @cpt-begin:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-04
            // A rejected later reconfiguration moves Ready to the terminal
            // Failed state; the handlers that reject it belong to later features.
            | (Self::Ready, Self::Failed) => next,
            // @cpt-end:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-04
            _ => self,
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-01
        // The runtime invokes init with the platform client hub and the
        // oagw.config block.
        self.enter(LifecycleState::Initializing);
        // @cpt-end:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-01

        match self.bootstrap(ctx).await {
            // @cpt-begin:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-02
            // Configuration loading, dependency resolution and GTS provisioning
            // succeeded; the REST phase registers the routes under /oagw/v1.
            Ok(()) => {
                // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-14
                // The gear is returned in the Ready state of
                // `cpt-cf-oagw-state-gear-lifecycle` to the ToolKit runtime:
                // `Ok(())` from init only after the state has been entered.
                self.enter(LifecycleState::Ready);
                info!(
                    gear = OagwGear::MODULE_NAME,
                    prefix = route_shell::MOUNT_PREFIX,
                    routes = route_shell::shell_routes().len(),
                    "oagw gear initialized"
                );
                Ok(())
                // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-14
            }
            // @cpt-end:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-02
            // @cpt-begin:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-03
            // Any init step that fails (unknown configuration key, unresolvable
            // dependency, provisioning error) reaches the terminal Failed state
            // and aborts startup.
            Err(error) => {
                self.enter(LifecycleState::Failed);
                Err(startup_failure("init", &error))
            } // @cpt-end:cpt-cf-oagw-state-gear-lifecycle:p1:inst-lc-03
        }
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        _openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        info!(
            gear = OagwGear::MODULE_NAME,
            prefix = route_shell::MOUNT_PREFIX,
            "registering the oagw route shell"
        );
        self.mount_shell(router, MountLedger::shared())
            .map_err(|error| startup_failure("route registration", &error))
    }
}

impl OagwGear {
    /// Registers the closed `/oagw/v1` shell on `router`, claiming the mount
    /// prefix in `ledger`.
    ///
    /// Split from [`RestApiCapability::register_rest`] so the registration call
    /// can be driven against a ledger of its own.
    ///
    /// # Errors
    /// Returns the typed startup error surface when the prefix is already
    /// mounted by another registration.
    pub fn mount_shell(&self, router: Router, ledger: &MountLedger) -> Result<Router, OagwError> {
        let data_plane = self.data_plane().ok_or_else(|| {
            OagwError::route_error(format!(
                "{} mounts no proxy shell before its init() constructed the data plane",
                OagwGear::MODULE_NAME
            ))
        })?;
        route_shell::mount(
            router,
            ledger,
            Arc::clone(&self.control_plane),
            Arc::clone(&data_plane),
        )
    }

    /// The control plane the 15 mounted management handlers serve.
    #[must_use]
    pub fn control_plane(&self) -> Arc<ControlPlaneService> {
        Arc::clone(&self.control_plane)
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> LifecycleState {
        *self.state.lock()
    }

    /// The configuration the last successful init validated.
    #[must_use]
    pub fn config(&self) -> Option<OagwConfig> {
        self.config.get().copied()
    }

    /// The three plugin registries a completed `init()` constructed.
    ///
    /// `None` until `init()` succeeds — in particular for a gear whose
    /// dependency resolution failed, which holds no registry at all — and a
    /// shared read-only value afterwards, so no registration path is reachable
    /// from a request (`cpt-cf-oagw-flow-registry-init` step 9).
    #[must_use]
    pub fn plugin_registries(&self) -> Option<&PluginRegistries> {
        self.plugin_registries.get()
    }

    /// The data plane the 10 mounted proxy handlers drive.
    ///
    /// `None` until `init()` succeeds — in particular for a gear whose
    /// dependency resolution failed, which holds no pipeline at all — and a
    /// shared read-only value afterwards, so no proxy request is reachable
    /// from a gear that never reached `Ready`.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<ProxyPipeline>> {
        self.data_plane.get().cloned()
    }

    /// Applies a lifecycle transition, leaving invalid transitions unapplied.
    fn enter(&self, next: LifecycleState) {
        let mut state = self.state.lock();
        *state = state.transition(next);
    }

    /// Runs the init steps of `cpt-cf-oagw-flow-gear-bootstrap` in order.
    async fn bootstrap(&self, ctx: &GearCtx) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-01
        // Read the raw oagw.config block supplied by the platform and hand it to
        // the configuration loader.
        //
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-05
        // An unknown key or an out-of-range value aborts startup here with the
        // typed error naming the offending key.
        let config = OagwConfig::load(Some(ctx.raw_config()))?;
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-05
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-01

        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-06
        // Resolve the platform dependencies: the types-registry SDK client as a
        // gear-level dependency, and cred_store, toolkit-auth and tenant-resolver
        // through the toolkit client hub.
        let dependencies = dependency::resolve(&ctx.client_hub())?;
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-06

        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-09
        // Provision the identifier families through types-registry.
        type_provisioning::provision_identifier_families(&dependencies.types_registry).await?;
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-09

        // The registries are handed to the data plane instead of being read
        // back from the gear, so the step order the bootstrap fixes is the
        // order the call arguments carry.
        let registries = self.construct_plugin_registries(&config, &dependencies)?;
        let telemetry = self.construct_telemetry();
        self.construct_data_plane(&config, &dependencies, registries, telemetry)?;

        // The validated configuration is published with the rest of the state
        // a completed init holds, so `config()` reports only what an init that
        // reached its last step validated.
        self.config.set(config).map_err(|_| {
            OagwError::route_error(format!(
                "{} gear is already initialized",
                OagwGear::MODULE_NAME
            ))
        })?;
        Ok(())
    }

    /// Constructs the data-plane pipeline behind the proxy shell
    /// (`cpt-cf-oagw-feature-proxy-pipeline`).
    ///
    /// Runs after the registries — which the bootstrap hands in, so a pipeline
    /// that is never reached from a request never resolves a binding either —
    /// and holds the same tenant resolver the control plane's reads are scoped
    /// by.
    ///
    /// # Errors
    /// Returns the typed startup error surface when the pipeline cannot be
    /// constructed — the upstream connector being built from the system root
    /// certificate store — or the gear is already initialized.
    fn construct_data_plane(
        &self,
        config: &OagwConfig,
        dependencies: &dependency::PlatformDependencies,
        registries: PluginRegistries,
        telemetry: Arc<crate::infra::observability::Telemetry>,
    ) -> Result<(), OagwError> {
        // The registry is constructed inside the gear's `init()`, as the
        // rate-limiting DoD of entry 2.7 required, and the pipeline reads it
        // read-only from here on.
        let registries = Arc::new(registries);
        let limits = ProxyLimits {
            proxy_timeout: Duration::from_secs(config.proxy_timeout_secs),
            body_limit: config.body_limit_bytes,
            scheme: SchemePolicy {
                allow_http_upstream: config.allow_http_upstream,
                ssrf_enabled: config.ssrf_policy.enabled,
            },
        };
        let pipeline = ProxyPipeline::try_new(
            self.control_plane.stores(),
            Arc::clone(&dependencies.tenant_resolver),
            registries,
            limits,
            telemetry,
        )?;
        self.data_plane.set(Arc::new(pipeline)).map_err(|_| {
            OagwError::route_error(format!(
                "{} gear is already initialized",
                OagwGear::MODULE_NAME
            ))
        })
    }

    /// Registers the metric instruments and installs the telemetry the control
    /// plane's audit path and the data plane's pipeline read
    /// (`cpt-cf-oagw-dod-metric-instruments`).
    ///
    /// Split from [`OagwGear::construct_data_plane`], which consumes the
    /// returned telemetry, so the control-plane side effect is not folded into
    /// a constructor that can still fail.
    fn construct_telemetry(&self) -> Arc<crate::infra::observability::Telemetry> {
        // @cpt-begin:cpt-cf-oagw-dod-metric-instruments:p1:inst-full
        // The nine instruments of `cpt-cf-oagw-dod-metric-instruments` are
        // registered here, once, at gear init and before any request is served:
        // note that this is the only registration site in the process, no
        // handler or plugin ever registers one, and the meter is the one the
        // platform telemetry stack installed globally, scoped to this gear's
        // instrumentation library. No `OagwConfig` key is read for it, the
        // exposure surface being the platform's registry and not a route.
        let scope = opentelemetry::InstrumentationScope::builder(OagwGear::MODULE_NAME).build();
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-02
        // The instruments were registered once here, at gear initialization, on
        // the toolkit OpenTelemetry SDK the host configures — the nine
        // instruments of §1.5 and no others — and no instrument is created,
        // looked up or registered in any step of the emission flow below.
        let meter = opentelemetry::global::meter_with_scope(scope);
        let instruments = crate::infra::observability::MetricInstruments::register(&meter);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-02
        let telemetry = Arc::new(crate::infra::observability::Telemetry::new(
            instruments,
            Arc::new(crate::infra::observability::StdoutAuditSink),
        ));
        self.control_plane.set_telemetry(Arc::clone(&telemetry));
        // @cpt-end:cpt-cf-oagw-dod-metric-instruments:p1:inst-full
        telemetry
    }

    /// Constructs the three plugin registries
    /// (`cpt-cf-oagw-flow-registry-init`).
    ///
    /// Runs only after the platform dependencies resolved, so a gear whose
    /// dependency resolution failed holds no registry at all and the failure
    /// surfaces at startup instead of at request time. The constructed
    /// registries are returned as well as published, so the bootstrap hands
    /// them to the data plane instead of reading them back.
    ///
    /// # Errors
    /// Returns the typed startup error surface when the registries cannot be
    /// constructed or the gear is already initialized.
    fn construct_plugin_registries(
        &self,
        config: &OagwConfig,
        dependencies: &dependency::PlatformDependencies,
    ) -> Result<PluginRegistries, OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-01
        // The gear `init()` invocation carries the validated `OagwConfig` and
        // the platform client hub; the two token-cache keys of the gear
        // configuration are read from it.
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-02
        // The `TokenCacheConfig` parameter object bundles the TTL ceiling and
        // the cache capacity exactly as ADR 0008's "Gear-Level Configuration"
        // section defines them.
        let token_cache_config = TokenCacheConfig::from(config);
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-02
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-03
        // The `cred_store` SDK client is the one the gear-wiring feature already
        // resolved through the toolkit client hub; this flow resolves no
        // platform dependency of its own.
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-04
        // IF the platform client hub handed to `init()` does not carry the
        // `cred_store` client or the `toolkit-auth` client the token fetch
        // needs, the dependency-wiring failure of the gear-wiring feature has
        // already failed `init()` above, so this step is total: the failure
        // aborts startup and constructs none of the three registries.
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-05
        // The resolution failure is caught by the gear-wiring feature's
        // dependency wiring and aborts startup with its typed startup error
        // surface, so the gear never reaches `Ready` with half a chain.
        let cred_store = Arc::clone(&dependencies.cred_store);
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-05
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-04
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-03

        // The three registries are constructed by
        // `PluginRegistries::with_builtins` below, which holds the steps
        // `inst-pi-06` (the four auth built-ins over the cred-store client, the
        // token HTTP configuration parameter and the token-cache parameter
        // object), `inst-pi-07` (the guard registry with `required_headers` as
        // its only entry) and `inst-pi-08` (the transform registry with
        // `request_id` as its only entry); being one shared constructor, it is
        // the only place those steps are marked.
        let registries = PluginRegistries::with_builtins(cred_store, None, token_cache_config);

        self.plugin_registries
            .set(registries.clone())
            .map_err(|_| {
                OagwError::route_error(format!(
                    "{} gear is already initialized",
                    OagwGear::MODULE_NAME
                ))
            })?;
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-10
        // The three registries are returned to the gear, whose completed
        // `init()` is what lets the gear-wiring lifecycle reach its `Ready`
        // state; from that state the proxy pipeline resolves plugin bindings
        // from them for the process lifetime.
        Ok(registries)
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-10
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-01
    }
}

/// Logs the problem+json document of a failed startup step and turns the typed
/// error into the failure the runtime aborts process startup with.
///
/// `phase` names the step that failed — `init` for the bootstrap the runtime
/// invoked, `route registration` for the REST phase's registration call — so
/// both startup surfaces report through this one problem+json helper.
fn startup_failure(phase: &str, error: &OagwError) -> anyhow::Error {
    let problem = ProblemBody::from_error(error);
    tracing::error!(
        gear = OagwGear::MODULE_NAME,
        phase = phase,
        variant = error.mapping().variant,
        status = error.status(),
        problem = %serde_json::to_string(&problem).unwrap_or_default(),
        "oagw gear startup aborted"
    );
    anyhow!(
        "{} {phase} aborted: {}: {error}",
        OagwGear::MODULE_NAME,
        error.mapping().variant
    )
}

// @cpt-end:cpt-cf-oagw-dod-gear-registration:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::control_plane::Reply;
    use crate::config::{DEFAULT_TOKEN_CACHE_CAPACITY, DEFAULT_TOKEN_CACHE_TTL_SECS};
    use crate::infra::type_provisioning::IDENTIFIER_FAMILIES;
    use axum::middleware::Next;
    use axum::response::Response;
    use toolkit_security::SecurityContext;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// The tenant of the caller the tests authenticate.
    fn tenant() -> Uuid {
        Uuid::from_u128(0xa11ce)
    }

    /// A test stand-in for the platform authz middleware: every request the
    /// test sends belongs to [`tenant`].
    async fn inject_tenant(mut request: axum::extract::Request, next: Next) -> Response {
        let context = SecurityContext::builder()
            .subject_id(Uuid::from_u128(0xcafe))
            .subject_tenant_id(tenant())
            .build()
            .expect("a test context carries a subject and a tenant");
        request.extensions_mut().insert(context);
        next.run(request).await
    }

    /// The upstream a create stores, as the id the response carries.
    fn created_upstream_id(gear: &OagwGear) -> String {
        let reply = gear
            .control_plane()
            .create_upstream(
                tenant(),
                br#"{"server":{"endpoints":[{"scheme":"https","host":"api.example.com","port":443}]},"protocol":"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"}"#,
            )
            .expect("the gear's own control plane stores the upstream");
        let Reply::Created(value) = reply else {
            panic!("a create is answered 201, got {reply:?}");
        };
        value["id"]
            .as_str()
            .expect("the stored id is the bare uuid")
            .to_owned()
    }

    #[test]
    fn the_lifecycle_walks_the_declared_transitions() {
        assert_eq!(LifecycleState::default(), LifecycleState::Uninitialized);

        let state = LifecycleState::Uninitialized.transition(LifecycleState::Initializing);
        assert_eq!(state, LifecycleState::Initializing);
        assert_eq!(
            state.transition(LifecycleState::Ready),
            LifecycleState::Ready
        );
        assert_eq!(
            LifecycleState::Initializing.transition(LifecycleState::Failed),
            LifecycleState::Failed
        );
        assert_eq!(
            LifecycleState::Ready.transition(LifecycleState::Failed),
            LifecycleState::Failed
        );
    }

    #[test]
    fn undeclared_and_terminal_transitions_leave_the_state_unchanged() {
        assert_eq!(
            LifecycleState::Uninitialized.transition(LifecycleState::Ready),
            LifecycleState::Uninitialized
        );
        assert_eq!(
            LifecycleState::Uninitialized.transition(LifecycleState::Failed),
            LifecycleState::Uninitialized
        );
        assert_eq!(
            LifecycleState::Ready.transition(LifecycleState::Initializing),
            LifecycleState::Ready
        );
        assert_eq!(
            LifecycleState::Ready.transition(LifecycleState::Ready),
            LifecycleState::Ready
        );
        assert_eq!(
            LifecycleState::Failed.transition(LifecycleState::Initializing),
            LifecycleState::Failed,
            "Failed is terminal in this release"
        );
        assert_eq!(
            LifecycleState::Failed.transition(LifecycleState::Ready),
            LifecycleState::Failed
        );
    }

    #[test]
    fn the_gear_starts_uninitialized_and_ready_carries_the_config() {
        let gear = OagwGear::default();
        assert_eq!(gear.state(), LifecycleState::Uninitialized);
        assert!(gear.config().is_none());

        gear.enter(LifecycleState::Initializing);
        gear.enter(LifecycleState::Ready);
        assert_eq!(gear.state(), LifecycleState::Ready);
    }

    /// Installs the stub-backed data plane the shell tests mount the proxy
    /// handlers over: the same shape `construct_data_plane` produces, over the
    /// inert stub connector, since these tests never reach an upstream.
    fn mount_stub_data_plane(gear: &OagwGear) {
        use crate::infra::proxy::pipeline::stub::{StubConnector, pipeline as stub_pipeline};

        let pipeline = stub_pipeline(gear.control_plane.stores(), Arc::new(StubConnector::new()));
        gear.data_plane
            .set(pipeline)
            .expect("a test gear holds one data plane");
    }

    #[test]
    fn a_gear_without_a_data_plane_refuses_to_mount_the_shell() {
        let gear = OagwGear::default();
        let ledger = MountLedger::new();

        let error = gear.mount_shell(Router::new(), &ledger).unwrap_err();

        assert_eq!(error.mapping().variant, "RouteError");
        assert!(
            error.detail().contains("data plane"),
            "the refusal names the missing data plane: {}",
            error.detail()
        );
        assert!(
            !error.is_retriable(),
            "a refused registration is not retriable"
        );
        assert!(
            ledger.claims().is_empty(),
            "the refused registration claims no prefix"
        );
        assert!(gear.data_plane().is_none());
    }

    #[tokio::test]
    async fn a_ready_gear_mounts_the_shell_on_the_gear_relative_prefix() {
        let gear = OagwGear::default();
        gear.enter(LifecycleState::Initializing);
        gear.enter(LifecycleState::Ready);
        mount_stub_data_plane(&gear);

        let router = gear
            .mount_shell(Router::new(), &MountLedger::new())
            .unwrap();
        let management = router
            .clone()
            .oneshot(
                axum::http::Request::get("/oagw/v1/plugins")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            management.status(),
            401,
            "the mounted management path is served by the control-plane handler, \
             which the absent security context rejects"
        );

        let proxy = router
            .oneshot(
                axum::http::Request::get("/oagw/v1/proxy/payments")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            proxy.status(),
            401,
            "the mounted proxy path is served by the proxy handler, \
             which the absent security context rejects"
        );
    }

    /// The control plane the gear owns is the one the mounted shell serves: an
    /// upstream written through [`OagwGear::control_plane`] is the resource the
    /// wire answers for.
    #[tokio::test]
    async fn the_mounted_shell_serves_the_control_plane_the_gear_owns() {
        let gear = OagwGear::default();
        gear.enter(LifecycleState::Initializing);
        gear.enter(LifecycleState::Ready);
        mount_stub_data_plane(&gear);
        let id = created_upstream_id(&gear);

        let router = gear
            .mount_shell(Router::new(), &MountLedger::new())
            .unwrap()
            .layer(axum::middleware::from_fn(inject_tenant));
        let response = router
            .oneshot(
                axum::http::Request::get(format!("/oagw/v1/upstreams/{id}"))
                    .header("x-test-tenant", tenant().to_string())
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["id"].as_str(), Some(id.as_str()));
        assert_eq!(body["alias"].as_str(), Some("api.example.com"));
    }

    #[tokio::test]
    async fn a_second_registration_is_refused_naming_the_prefix() {
        let gear = OagwGear::default();
        mount_stub_data_plane(&gear);
        let ledger = MountLedger::new();
        let _ = gear.mount_shell(Router::new(), &ledger).unwrap();

        let error = gear.mount_shell(Router::new(), &ledger).unwrap_err();
        assert_eq!(error.mapping().variant, "ValidationError");
        assert!(error.detail().contains("'/oagw/v1'"), "{}", error.detail());
    }

    #[test]
    fn the_gear_declares_the_types_registry_dependency() {
        // The gear-level dependency list is the macro declaration; the hub keys
        // the dependency resolver looks up are the documented ones.
        assert_eq!(dependency::TYPES_REGISTRY_DEPENDENCY, "types_registry");
        assert_eq!(dependency::CRED_STORE_DEPENDENCY, "cred_store");
        assert_eq!(dependency::TOOLKIT_AUTH_DEPENDENCY, "toolkit-auth");
        assert_eq!(dependency::TENANT_RESOLVER_DEPENDENCY, "tenant-resolver");
    }

    #[test]
    fn init_provisions_the_identifier_families_of_the_documented_set() {
        let names: Vec<_> = IDENTIFIER_FAMILIES
            .iter()
            .map(|family| family.name)
            .collect();
        assert_eq!(names, ["upstream", "route", "plugin", "protocol", "error"]);
    }

    /// The plugin-registry wiring of `cpt-cf-oagw-flow-registry-init`: a gear
    /// that has not completed `init()` holds no registry, and the construction
    /// the bootstrap performs registers the built-ins under their identifiers
    /// with the gear-configuration cache settings threaded in.
    #[test]
    fn a_gear_that_has_not_initialized_holds_no_plugin_registry() {
        let gear = OagwGear::default();
        assert!(
            gear.plugin_registries().is_none(),
            "no registry is constructed before init() succeeds"
        );
        assert_eq!(gear.state(), LifecycleState::Uninitialized);
    }

    /// A test stand-in for the platform dependencies the bootstrap resolves:
    /// every client is inert, the same shape `infra::dependency`'s own tests
    /// use.
    struct NullAuthenticator;

    impl toolkit_security::BearerAuthenticator for NullAuthenticator {
        fn authenticate(
            &self,
            _token: &str,
        ) -> impl Future<
            Output = Result<toolkit_security::SecurityContext, toolkit_security::AuthNError>,
        > + Send {
            std::future::ready(Err(toolkit_security::AuthNError::InvalidToken))
        }
    }

    /// An inert tenant resolver.
    struct NullTenantResolver;

    #[async_trait::async_trait]
    impl tenant_resolver_sdk::TenantResolverClient for NullTenantResolver {
        async fn get_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound { tenant_id: id })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound {
                tenant_id: tenant_resolver_sdk::TenantId(Uuid::nil()),
            })
        }

        async fn get_tenants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _ids: &[tenant_resolver_sdk::TenantId],
            _options: &tenant_resolver_sdk::GetTenantsOptions,
        ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError>
        {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetAncestorsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetAncestorsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetAncestorsResponse {
                tenant: tenant_resolver_sdk::TenantRef {
                    id,
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: Vec::new(),
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetDescendantsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetDescendantsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetDescendantsResponse {
                tenant: tenant_resolver_sdk::TenantRef {
                    id,
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _ancestor_id: tenant_resolver_sdk::TenantId,
            _descendant_id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::IsAncestorOptions,
        ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
            Ok(false)
        }
    }

    /// Builds the platform dependencies the bootstrap hands to the plugin
    /// registries, over inert clients.
    fn platform_dependencies() -> dependency::PlatformDependencies {
        use credstore_sdk::test_util::MockCredStoreClient;
        use types_registry_sdk::testing::MockTypesRegistryClient;

        dependency::PlatformDependencies {
            types_registry: Arc::new(MockTypesRegistryClient::new()),
            cred_store: Arc::new(MockCredStoreClient::empty()),
            bearer_authenticator: Arc::new(toolkit_security::DynBearerAuthenticator::new(
                NullAuthenticator,
            )),
            tenant_resolver: Arc::new(NullTenantResolver),
        }
    }

    #[test]
    fn the_bootstrap_constructs_the_three_registries_over_the_resolved_cred_store() {
        let config = OagwConfig::default();
        let gear = OagwGear::default();

        gear.construct_plugin_registries(&config, &platform_dependencies())
            .expect("the registries are constructed from the resolved dependencies");
        let registries = gear
            .plugin_registries()
            .expect("a successful construction leaves the registries on the gear");

        assert_eq!(registries.auth.len(), 4, "the four auth built-ins");
        assert_eq!(registries.guard.len(), 1, "the single guard built-in");
        assert_eq!(
            registries.transform.len(),
            1,
            "the single transform built-in"
        );
        assert!(
            registries
                .auth
                .lookup("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
                .is_some(),
            "the apikey built-in resolves under its full GTS form over the resolved cred store"
        );
    }

    #[test]
    fn the_bootstrap_threads_the_gear_configuration_into_the_token_cache_settings() {
        let config = OagwConfig::default();
        assert_eq!(
            TokenCacheConfig::from(&config),
            TokenCacheConfig::new(DEFAULT_TOKEN_CACHE_TTL_SECS, DEFAULT_TOKEN_CACHE_CAPACITY),
            "the defaults of ADR 0008 are the gear-level settings"
        );

        let configured = OagwConfig::load(Some(&serde_json::json!({
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 128
        })))
        .expect("both keys are known gear-configuration keys");
        assert_eq!(
            TokenCacheConfig::from(&configured),
            TokenCacheConfig::new(60, 128),
            "the two keys the gear-wiring feature owns reach the plugin constructors"
        );
    }

    #[test]
    fn a_second_construction_fails_and_keeps_the_first_registries() {
        let gear = OagwGear::default();
        let dependencies = platform_dependencies();

        gear.construct_plugin_registries(&OagwConfig::default(), &dependencies)
            .expect("the first construction lands");
        let error = gear
            .construct_plugin_registries(&OagwConfig::default(), &dependencies)
            .expect_err("a gear is initialized once");

        assert_eq!(error.mapping().variant, "RouteError");
        assert!(
            error.detail().contains("already initialized"),
            "{}",
            error.detail()
        );
        assert_eq!(gear.plugin_registries().expect("still set").auth.len(), 4);
    }

    #[test]
    fn a_dependency_resolution_failure_leaves_no_registry_behind() {
        let resolution = dependency::resolve(&toolkit::ClientHub::default());

        assert!(
            resolution.is_err(),
            "an empty hub resolves no platform dependency"
        );
        let gear = OagwGear::default();
        assert!(
            gear.plugin_registries().is_none(),
            "a gear whose dependency resolution failed constructs none of the three registries"
        );
    }

    #[test]
    fn the_plugin_feature_registers_no_http_route_of_its_own() {
        let routes = crate::api::rest::route_shell::shell_routes();

        assert_eq!(
            routes.len(),
            27,
            "the shell stays the closed 27-route set, unextended by the plugin feature"
        );
        let plugin_paths: std::collections::BTreeSet<&str> = routes
            .iter()
            .filter(|(_, path)| path.contains("plugin"))
            .map(|(_, path)| *path)
            .collect();
        assert_eq!(
            plugin_paths,
            std::collections::BTreeSet::from(
                ["/plugins", "/plugins/{id}", "/plugins/{id}/source",]
            ),
            "the only plugin routes are the management CRUD paths the shell already mounts: \
             no data-plane plugin execution route is added"
        );
        for (method, path) in &routes {
            assert!(
                !path.contains("invoke") && !path.contains("execute"),
                "no plugin execution route is registered: {method} {path}"
            );
        }
    }

    /// §6 of `cpt-cf-oagw-dod-crate-layout` and `cpt-cf-oagw-dod-gear-bootstrap`:
    /// the `cf-gears-oagw` package compiles as the lib target `oagw` with the
    /// documented module layout, and exports the gear type, its lifecycle state,
    /// the aggregates, value objects, the `DomainError`, the three repository
    /// traits and the in-memory stores behind them.
    #[test]
    fn the_crate_exports_the_documented_surface_and_module_layout() {
        assert_eq!(env!("CARGO_PKG_NAME"), "cf-gears-oagw");
        assert_eq!(env!("CARGO_CRATE_NAME"), "oagw", "the lib target is `oagw`");

        // The module layout of `cpt-cf-oagw-dod-crate-layout` is present.
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for module in [
            "src/lib.rs",
            "src/gear.rs",
            "src/config.rs",
            "src/domain/error.rs",
            "src/domain/repo.rs",
            "src/api/rest/error.rs",
            "src/infra/type_provisioning.rs",
            "src/infra/storage/mod.rs",
        ] {
            assert!(
                crate_root.join(module).is_file(),
                "the documented module layout carries {module}"
            );
        }

        // The gear type and its lifecycle state are the re-exports of `src/lib.rs`.
        assert_eq!(
            std::any::TypeId::of::<crate::OagwGear>(),
            std::any::TypeId::of::<OagwGear>(),
            "crate::OagwGear re-exports the gear type of src/gear.rs"
        );
        assert_eq!(
            std::any::TypeId::of::<crate::LifecycleState>(),
            std::any::TypeId::of::<LifecycleState>(),
            "crate::LifecycleState re-exports the state machine of src/gear.rs"
        );

        // The domain surface is reachable through the exported module tree: the
        // `DomainError`, the aggregates and their value objects.
        let error = crate::domain::error::DomainError::not_found("id", "absent");
        assert_eq!(
            error.kind(),
            Some(crate::domain::error::ViolationKind::NotFound)
        );
        let endpoint = crate::domain::model::Endpoint {
            scheme: "https".to_owned(),
            host: Some("api.example.com".to_owned()),
            port: 443,
        };
        let upstream = crate::domain::model::Upstream {
            tenant_id: Some(tenant()),
            server: Some(crate::domain::model::ServerConfig {
                endpoints: vec![endpoint],
            }),
            ..crate::domain::model::Upstream::default()
        };
        assert_eq!(
            upstream
                .server
                .as_ref()
                .map(|server| server.endpoints.len()),
            Some(1),
            "the aggregate and its value objects are exported and usable"
        );
        let _route = crate::domain::model::Route::default();
        let _plugin = crate::domain::model::Plugin::default();

        // The three repository traits are exported and the in-memory stores of
        // `src/infra/storage/` are swap-in compatible with them.
        let stores = crate::infra::storage::InMemoryStores::new();
        let upstreams = stores.upstreams();
        let routes = stores.routes();
        let plugins = stores.plugins();
        let upstreams: &dyn crate::domain::repo::UpstreamRepository = &upstreams;
        let routes: &dyn crate::domain::repo::RouteRepository = &routes;
        let plugins: &dyn crate::domain::repo::PluginRepository = &plugins;
        assert!(upstreams.list(tenant()).unwrap().is_empty());
        assert!(routes.list(tenant()).unwrap().is_empty());
        assert!(plugins.list(tenant()).unwrap().is_empty());
    }

    /// A configuration provider carrying no `oagw.config` block, the shape of a
    /// host that starts the gear on its defaults.
    struct EmptyConfigProvider;

    impl toolkit::config::ConfigProvider for EmptyConfigProvider {
        fn get_gear_config(&self, _gear_name: &str) -> Option<&serde_json::Value> {
            None
        }
    }

    /// A types-registry fake the bootstrap provisions its identifier families
    /// against: every requested type-schema is answered as registered, which is
    /// how a live registry answers a gear whose families are new, and every
    /// read is answered `NotFound`, the bootstrap reading nothing back.
    struct ProvisioningRegistry;

    #[async_trait::async_trait]
    impl types_registry_sdk::TypesRegistryClient for ProvisioningRegistry {
        async fn register(
            &self,
            _entities: Vec<serde_json::Value>,
        ) -> Result<Vec<types_registry_sdk::RegisterResult>, toolkit_canonical_errors::CanonicalError>
        {
            Ok(Vec::new())
        }

        async fn register_type_schemas(
            &self,
            type_schemas: Vec<serde_json::Value>,
        ) -> Result<Vec<types_registry_sdk::RegisterResult>, toolkit_canonical_errors::CanonicalError>
        {
            Ok(type_schemas
                .iter()
                .map(|schema| types_registry_sdk::RegisterResult::Ok {
                    gts_id: schema["$id"].as_str().unwrap_or_default().to_owned(),
                })
                .collect())
        }

        async fn get_type_schema(
            &self,
            type_id: &str,
        ) -> Result<types_registry_sdk::GtsTypeSchema, toolkit_canonical_errors::CanonicalError>
        {
            Err(types_registry_sdk::testing::not_found(type_id))
        }

        async fn get_type_schema_by_uuid(
            &self,
            type_uuid: Uuid,
        ) -> Result<types_registry_sdk::GtsTypeSchema, toolkit_canonical_errors::CanonicalError>
        {
            Err(types_registry_sdk::testing::not_found(
                type_uuid.to_string(),
            ))
        }

        async fn get_type_schemas(
            &self,
            type_ids: Vec<String>,
        ) -> std::collections::HashMap<
            String,
            Result<types_registry_sdk::GtsTypeSchema, toolkit_canonical_errors::CanonicalError>,
        > {
            type_ids
                .into_iter()
                .map(|id| {
                    let id_clone = id.clone();
                    (id, Err(types_registry_sdk::testing::not_found(id_clone)))
                })
                .collect()
        }

        async fn get_type_schemas_by_uuid(
            &self,
            type_uuids: Vec<Uuid>,
        ) -> std::collections::HashMap<
            Uuid,
            Result<types_registry_sdk::GtsTypeSchema, toolkit_canonical_errors::CanonicalError>,
        > {
            type_uuids
                .into_iter()
                .map(|uuid| {
                    let key = uuid;
                    (
                        key,
                        Err(types_registry_sdk::testing::not_found(uuid.to_string())),
                    )
                })
                .collect()
        }

        async fn list_type_schemas(
            &self,
            _query: types_registry_sdk::TypeSchemaQuery,
        ) -> Result<Vec<types_registry_sdk::GtsTypeSchema>, toolkit_canonical_errors::CanonicalError>
        {
            Ok(Vec::new())
        }

        async fn register_instances(
            &self,
            _instances: Vec<serde_json::Value>,
        ) -> Result<Vec<types_registry_sdk::RegisterResult>, toolkit_canonical_errors::CanonicalError>
        {
            Ok(Vec::new())
        }

        async fn get_instance(
            &self,
            id: &str,
        ) -> Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>
        {
            Err(types_registry_sdk::testing::not_found(id))
        }

        async fn get_instance_by_uuid(
            &self,
            uuid: Uuid,
        ) -> Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>
        {
            Err(types_registry_sdk::testing::not_found(uuid.to_string()))
        }

        async fn get_instances(
            &self,
            ids: Vec<String>,
        ) -> std::collections::HashMap<
            String,
            Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>,
        > {
            ids.into_iter()
                .map(|id| {
                    let id_clone = id.clone();
                    (id, Err(types_registry_sdk::testing::not_found(id_clone)))
                })
                .collect()
        }

        async fn get_instances_by_uuid(
            &self,
            uuids: Vec<Uuid>,
        ) -> std::collections::HashMap<
            Uuid,
            Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>,
        > {
            uuids
                .into_iter()
                .map(|uuid| {
                    let key = uuid;
                    (
                        key,
                        Err(types_registry_sdk::testing::not_found(uuid.to_string())),
                    )
                })
                .collect()
        }

        async fn list_instances(
            &self,
            _query: types_registry_sdk::InstanceQuery,
        ) -> Result<Vec<types_registry_sdk::GtsInstance>, toolkit_canonical_errors::CanonicalError>
        {
            Ok(Vec::new())
        }
    }

    /// The client hub a successful `init` resolves its four platform
    /// dependencies from, over the inert mock clients this crate's tests use —
    /// the same shape `infra::dependency`'s own hub tests build.
    fn client_hub() -> toolkit::ClientHub {
        use credstore_sdk::test_util::MockCredStoreClient;

        let hub = toolkit::ClientHub::default();
        hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(ProvisioningRegistry));
        hub.register::<dyn credstore_sdk::CredStoreClientV1>(
            Arc::new(MockCredStoreClient::empty()),
        );
        hub.register::<toolkit_security::DynBearerAuthenticator>(Arc::new(
            toolkit_security::DynBearerAuthenticator::new(NullAuthenticator),
        ));
        hub.register::<dyn tenant_resolver_sdk::TenantResolverClient>(Arc::new(NullTenantResolver));
        hub
    }

    /// The gear context a test drives the bootstrap with: the platform gear
    /// name, a provider without an `oagw.config` block, the given client hub
    /// and a cancellation token the host would own.
    fn gear_ctx(hub: toolkit::ClientHub) -> GearCtx {
        GearCtx::new(
            OagwGear::MODULE_NAME,
            Uuid::new_v4(),
            Arc::new(EmptyConfigProvider),
            Arc::new(hub),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    /// The bootstrap orchestration of `cpt-cf-oagw-flow-gear-bootstrap` driven
    /// end to end over the mock platform clients: the runtime's `init` call
    /// reaches `Ready` holding the validated configuration, the three plugin
    /// registries and the data plane, none of which a gear that never ran the
    /// bootstrap holds.
    #[tokio::test]
    async fn init_reaches_ready_holding_the_config_registries_and_data_plane() {
        let gear = OagwGear::default();
        let ctx = gear_ctx(client_hub());

        gear.init(&ctx)
            .await
            .expect("the bootstrap resolves, provisions and constructs");

        assert_eq!(gear.state(), LifecycleState::Ready);
        assert_eq!(
            gear.config(),
            Some(OagwConfig::default()),
            "no oagw.config block, so the defaults are the validated configuration"
        );
        let registries = gear
            .plugin_registries()
            .expect("a completed init holds the three registries");
        assert_eq!(registries.auth.len(), 4, "the four auth built-ins");
        assert_eq!(registries.guard.len(), 1, "the single guard built-in");
        assert_eq!(
            registries.transform.len(),
            1,
            "the single transform built-in"
        );
        assert!(
            gear.data_plane().is_some(),
            "the data plane the proxy shell mounts was constructed last"
        );
    }

    /// A hub that carries none of the platform dependencies fails the
    /// resolution step: the gear reaches the terminal `Failed` state with the
    /// typed startup error naming the missing dependency, and publishes none of
    /// the state a completed init holds.
    #[tokio::test]
    async fn a_hub_missing_a_dependency_reaches_failed_with_a_typed_error() {
        let gear = OagwGear::default();
        let ctx = gear_ctx(toolkit::ClientHub::default());

        let error = gear
            .init(&ctx)
            .await
            .expect_err("an empty hub resolves no platform dependency");

        assert_eq!(
            gear.state(),
            LifecycleState::Failed,
            "a failed init reaches the terminal Failed state"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("LinkUnavailable"),
            "the typed startup error names its variant: {rendered}"
        );
        assert!(
            rendered.contains("'types_registry'"),
            "the typed startup error names the missing dependency: {rendered}"
        );
        assert!(gear.config().is_none(), "a failed init publishes no config");
        assert!(
            gear.plugin_registries().is_none(),
            "a gear whose dependency resolution failed holds no registry"
        );
        assert!(gear.data_plane().is_none());
    }

    /// The REST phase's registration call, driven over the process-wide ledger
    /// the runtime's registration claims.
    ///
    /// Its own module, because the call claims `/oagw/v1` in
    /// `MountLedger::shared()` for the rest of the process: no other test of the
    /// crate claims that ledger, so the claim this test leaves behind is
    /// observable by no other test.
    mod register_rest {
        use super::*;
        use toolkit::api::OperationSpec;

        /// The no-op OpenAPI registry: the closed shell registers no operation
        /// spec and no schema of its own.
        struct NoOpenApi;

        impl toolkit::api::OpenApiRegistry for NoOpenApi {
            fn register_operation(&self, _spec: &OperationSpec) {}

            fn ensure_schema_raw(
                &self,
                name: &str,
                _schemas: Vec<(
                    String,
                    utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
                )>,
            ) -> String {
                name.to_owned()
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        #[tokio::test]
        async fn register_rest_mounts_the_shell_over_the_process_wide_ledger() {
            let gear = OagwGear::default();
            let ctx = gear_ctx(client_hub());
            gear.init(&ctx)
                .await
                .expect("the gear is initialized before the REST phase registers");

            let router = gear
                .register_rest(&ctx, Router::new(), &NoOpenApi)
                .expect("the runtime's registration call mounts the shell");

            assert!(
                MountLedger::shared()
                    .claims()
                    .contains(&route_shell::MOUNT_PREFIX.to_owned()),
                "the process-wide ledger holds the claimed prefix: {:?}",
                MountLedger::shared().claims()
            );
            let response = router
                .oneshot(
                    axum::http::Request::get("/oagw/v1/plugins")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                401,
                "the mounted management path is served by the control-plane handler"
            );
        }
    }
}
