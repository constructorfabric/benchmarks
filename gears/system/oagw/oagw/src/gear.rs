//! Gear declaration for the OAGW gear.
//!
//! Realizes `cpt-cf-oagw-flow-gear-init`, `cpt-cf-oagw-state-gear-foundation-lifecycle`
//! and `cpt-cf-oagw-dod-gear-registration`. The gear depends on
//! `types-registry` (topologically ordered by the runtime), loads its config in
//! `init`, provisions the GTS type catalogue in `post_init` — failing closed on
//! any refusal — and mounts the management surface on `/oagw/v1` in
//! `register_rest`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{RestApiCapability, SystemCapability};
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::MOUNT_POINT;
use crate::api::rest::register_management_routes;
use crate::api::rest::state::OagwState;
use crate::config::{ConfigError, OagwConfig};
use crate::gts::provisioning::provision;

/// OAGW gear.
///
/// ## Capabilities
///
/// - `system` — provisioned during startup, before the data plane answers
/// - `rest` — mounts the ten management paths on `/oagw/v1`
///
/// ## Dependencies
///
/// - `types_registry` — the `ClientHub` supplier of the
///   [`TypesRegistryClient`] the type catalogue is provisioned through.
///
/// The `AuthZ` resolver is resolved from the `ClientHub` opportunistically and
/// is **not** a declared dependency: a process that starts without it mounts a
/// management surface that answers 403 to every request, rather than failing
/// the startup or, worse, serving an unenforced surface.
// @cpt-dod:cpt-cf-oagw-dod-gear-registration:p1
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest],
    deps = [types_registry]
)]
pub struct OagwGear {
    // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-declare-config
    config: OnceLock<OagwConfig>,
    // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-declare-config
    registry: OnceLock<Arc<dyn TypesRegistryClient>>,
    state: OnceLock<Arc<OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            registry: OnceLock::new(),
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The configuration the gear started with, once `init` has run.
    #[must_use]
    pub fn config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// The registry client resolved from the `ClientHub`, once `init` has run.
    #[must_use]
    pub fn registry(&self) -> Option<&Arc<dyn TypesRegistryClient>> {
        self.registry.get()
    }

    /// The shared state the mounted management surface serves, once
    /// `register_rest` has run.
    #[must_use]
    pub fn state(&self) -> Option<&Arc<OagwState>> {
        self.state.get()
    }

    /// Loads the gear configuration, applies its validation, and stores it.
    fn load_config(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-load-config
        let cfg: OagwConfig = ctx
            .config_or_default::<OagwConfig>()
            .map_err(|e| anyhow::anyhow!("oagw configuration could not be loaded: {e}"))?;
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-load-config

        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-validate
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-abort
        cfg.validate().map_err(config_error)?;
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-abort
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-validate

        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-else
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-init-ok
        self.config
            .set(cfg)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-init-ok
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-else
        Ok(())
    }

    /// Resolves the types-registry client the catalogue is provisioned through.
    fn resolve_registry(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-resolve-registry
        let client = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| {
                anyhow::anyhow!(
                    "oagw: the types-registry client is not available from the ClientHub: {e}"
                )
            })?;
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-resolve-registry

        self.registry
            .set(client)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

/// Turns a [`ConfigError`] into an `anyhow` error that names the offending key.
fn config_error(error: ConfigError) -> anyhow::Error {
    anyhow::anyhow!("oagw configuration rejected: {error}")
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-runtime-call
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-init-fail
        self.load_config(ctx)?;
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-init-fail
        self.resolve_registry(ctx)?;
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-runtime-call
        tracing::info!(
            gear = Self::MODULE_NAME,
            "OAGW gear initialized: configuration loaded and types-registry client resolved"
        );
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-return
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-return
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    /// Provisions the GTS type catalogue.
    ///
    /// Runs after every gear has initialized, so the types-registry client is
    /// available. Any per-entry refusal or catastrophic SDK failure fails the
    /// startup: the gear never reports readiness with an unprovisioned
    /// catalogue.
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-post-init-phase
        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-registry-init
        let client = self
            .registry
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw: types-registry client not initialized"))?;
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-registry-init
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-post-init-phase

        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-enumerate
        let provisioned = provision(client.as_ref())
            .await
            // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-else
            // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-provision-fail
            .map_err(|e| anyhow::anyhow!("oagw: GTS type catalogue provisioning failed: {e}"))?;
            // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-provision-fail
            // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-else
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-enumerate

        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-provisioned
        tracing::info!(
            total = provisioned.total,
            succeeded = provisioned.succeeded,
            "OAGW type catalogue provisioned; gear state advanced to type-catalog-provisioned"
        );
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-provisioned

        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-return
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-return
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        _openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        if self.state.get().is_none() {
            let state = self.assemble_state(ctx)?;
            self.state
                .set(state)
                .map_err(|_| anyhow::anyhow!("{} gear already mounted", Self::MODULE_NAME))?;
        }
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw: the management surface was not assembled"))?;
        // @cpt-begin:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-mount
        let mounted = register_management_routes(router, Arc::clone(state));
        // @cpt-end:cpt-cf-oagw-flow-gear-init:p1:inst-gear-init-mount

        // @cpt-begin:cpt-cf-oagw-dod-management-routes:p1:inst-mgmt-routes-mount
        tracing::debug!(
            mount_point = MOUNT_POINT,
            enforcer = state.enforcer().is_some(),
            "OAGW management surface mounted"
        );
        // @cpt-end:cpt-cf-oagw-dod-management-routes:p1:inst-mgmt-routes-mount
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-ready
        Ok(mounted)
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-ready
    }
}

impl OagwGear {
    /// Assembles the shared state the management surface serves.
    ///
    /// The configuration `init` loaded is the one compiled here, and the
    /// `AuthZ` client is resolved from the `ClientHub` when the hub carries
    /// one: a resolver that never started leaves the enforcer absent, and the
    /// mounted surface answers 403 to every management request.
    fn assemble_state(&self, ctx: &GearCtx) -> anyhow::Result<Arc<OagwState>> {
        let config = self
            .config
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw: the configuration is not loaded; `register_rest` runs after `init`"))?;
        let authz = ctx.client_hub().get::<dyn authz_resolver_sdk::api::AuthZResolverClient>();
        if let Err(error) = &authz {
            // @cpt-begin:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-absent
            tracing::warn!(
                error = %error,
                "no AuthZ resolver resolved from the ClientHub; the OAGW management surface fails closed"
            );
            // @cpt-end:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-absent
        }
        let resolver =
            ctx.client_hub().get::<dyn tenant_resolver_sdk::TenantResolverClient>();
        if let Err(error) = &resolver {
            tracing::warn!(
                error = %error,
                "no tenant-resolver resolved from the ClientHub; every bind and every resolution fails closed"
            );
        }
        let cred_store = ctx.client_hub().get::<dyn credstore_sdk::CredStoreClientV1>();
        if let Err(error) = &cred_store {
            tracing::warn!(
                error = %error,
                "no credential store resolved from the ClientHub; every credential resolution fails closed"
            );
        }
        OagwState::assemble(config, authz.ok(), resolver.ok(), cred_store.ok())
            .map(Arc::new)
            .map_err(|error| anyhow::anyhow!("oagw: the management surface could not be assembled: {error}"))
    }
}
