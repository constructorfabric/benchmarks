//! Gear declaration for the OAGW (Outbound API Gateway) gear
//! (`cpt-cf-oagw-dod-gear-registration`).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::{Extension, Router};
use credstore_sdk::CredStoreClientV1;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::state::ControlPlaneState;

/// OAGW gear: outbound API gateway foundation.
///
/// ## Capabilities
///
/// - `rest` — mounts the gear-relative router at `/oagw/v1/...`
///   (`cpt-cf-oagw-dod-router-mount`)
///
/// `deps = [credstore]` (`cpt-cf-oagw-feature-plugin-runtime`): the
/// `apikey`/`oauth2_client_cred{,_basic}` built-in auth plugins resolve
/// credentials exclusively through the credential store
/// (`cpt-cf-oagw-nfr-credential-isolation`), fetched from the `ClientHub`
/// once at `init` and shared for the life of the process, the same pattern
/// every other cross-gear dependency in this workspace uses.
#[toolkit::gear(name = "oagw", deps = [credstore], capabilities = [rest])]
pub struct OagwGear {
    /// Resolved once at startup (`cpt-cf-oagw-dod-config-resolution`) and
    /// held for the life of the process.
    config: OnceLock<Arc<OagwConfig>>,
    /// In-process control-plane store (`cpt-cf-oagw-dod-control-plane-state`).
    state: OnceLock<Arc<ControlPlaneState>>,
    /// Shared outbound HTTP client for the proxy data plane
    /// (`cpt-cf-oagw-dod-proxy-timeout`), built once at startup with a
    /// connect timeout matching `proxy_timeout_secs`.
    http_client: OnceLock<Arc<reqwest::Client>>,
    /// The credential-store client, resolved from the `ClientHub` once at
    /// startup (`cpt-cf-oagw-dod-credential-isolation`).
    credstore: OnceLock<Arc<dyn CredStoreClientV1>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            state: OnceLock::new(),
            http_client: OnceLock::new(),
            credstore: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The resolved gear configuration, if `init` has run.
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.config.get().cloned()
    }

    /// The in-process control-plane store, if `init` has run.
    #[must_use]
    pub fn state(&self) -> Option<Arc<ControlPlaneState>> {
        self.state.get().cloned()
    }

    /// The shared outbound HTTP client, if `init` has run.
    #[must_use]
    pub fn http_client(&self) -> Option<Arc<reqwest::Client>> {
        self.http_client.get().cloned()
    }

    /// The credential-store client, if `init` has run.
    #[must_use]
    pub fn credstore(&self) -> Option<Arc<dyn CredStoreClientV1>> {
        self.credstore.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    /// Resolves the gear's configuration and initializes its in-process
    /// control-plane state.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration section fails to parse or
    /// validate, or if `init` runs more than once for this gear instance.
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-return-01
        let cfg: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_policy_enabled = cfg.ssrf_policy.enabled,
            "oagw: resolved gear configuration"
        );
        let proxy_timeout_secs = cfg.proxy_timeout_secs;
        let token_cache_capacity = cfg.token_cache_capacity;
        self.config
            .set(Arc::new(cfg))
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        // @cpt-end:cpt-cf-oagw-dod-config-resolution:p1:inst-config-res-return-01

        // @cpt-begin:cpt-cf-oagw-dod-control-plane-state:p2:inst-cp-init-initialize-01
        self.state
            .set(Arc::new(ControlPlaneState::with_token_cache_capacity(
                token_cache_capacity,
            )))
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        // @cpt-end:cpt-cf-oagw-dod-control-plane-state:p2:inst-cp-init-initialize-01

        // @cpt-begin:cpt-cf-oagw-dod-proxy-timeout:p1:inst-http-client-init-01
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(proxy_timeout_secs.max(1)))
            .build()
            .map_err(|error| {
                anyhow::anyhow!("oagw: failed to build outbound HTTP client: {error}")
            })?;
        self.http_client
            .set(Arc::new(client))
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        // @cpt-end:cpt-cf-oagw-dod-proxy-timeout:p1:inst-http-client-init-01

        // @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-gear-credstore-init-01
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|error| {
                anyhow::anyhow!("oagw: credential-store client unavailable: {error}")
            })?;
        self.credstore
            .set(credstore)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        // @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-gear-credstore-init-01

        info!("oagw gear initialized");
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    /// Merges this gear's routes onto the router it is given and attaches
    /// the control-plane store as a request extension for later features'
    /// handlers.
    ///
    /// # Errors
    ///
    /// Returns an error if `init` has not run yet, so the control-plane
    /// store is not available.
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: control-plane state not initialized"))?;
        let config = self
            .config
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: config not initialized"))?;
        let http_client = self
            .http_client
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: HTTP client not initialized"))?;
        let credstore = self
            .credstore
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: credential-store client not initialized"))?;

        info!("oagw: registering gear-relative REST routes at /oagw/v1");
        let router = routes::register_routes(router, openapi);

        Ok(router
            .layer(Extension(state))
            .layer(Extension(config))
            .layer(Extension(http_client))
            .layer(Extension(credstore)))
    }
}

#[cfg(test)]
mod tests {
    use super::OagwGear;
    use credstore_sdk::CredStoreClientV1;
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::sync::Arc;
    use toolkit::api::openapi_registry::OpenApiRegistryImpl;
    use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
    use uuid::Uuid;

    /// Minimal `ConfigProvider` returning a fixed JSON section for the
    /// `oagw` gear, mirroring the shape the real config loader hands to
    /// `GearCtx::config_or_default` (`{"config": {...}}`).
    struct FixedConfigProvider {
        section: Option<serde_json::Value>,
    }

    impl ConfigProvider for FixedConfigProvider {
        fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
            if gear_name == "oagw" {
                self.section.as_ref()
            } else {
                None
            }
        }
    }

    /// A `ClientHub` with an empty `CredStoreClientV1` double already
    /// registered, matching the real deployment's `deps = [credstore]`
    /// wiring (`cpt-cf-oagw-dod-credential-isolation`) closely enough for
    /// `init` to succeed without a real credstore gear present.
    fn hub_with_credstore() -> ClientHub {
        let hub = ClientHub::default();
        let client: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());
        hub.register::<dyn CredStoreClientV1>(client);
        hub
    }

    // reason: `Default::default()` below is inferred as
    // `tokio_util::sync::CancellationToken::default()` from `GearCtx::new`'s
    // signature; it is deliberately not named directly since this crate does
    // not take `tokio-util` as a direct dependency, and adding one purely to
    // spell out a test-only token's type would be a needless new dependency.
    #[allow(clippy::default_trait_access)]
    fn test_ctx(section: Option<serde_json::Value>) -> GearCtx {
        GearCtx::new(
            "oagw",
            Uuid::new_v4(),
            Arc::new(FixedConfigProvider { section }),
            Arc::new(hub_with_credstore()),
            Default::default(),
        )
    }

    /// A `GearCtx` with no `CredStoreClientV1` registered, for the one test
    /// asserting `init` fails when the dependency is absent
    /// (`cpt-cf-oagw-dod-credential-isolation`).
    // reason: see `test_ctx`'s `#[allow(clippy::default_trait_access)]` note;
    // the trailing `Default::default()` here is the same inferred
    // `tokio_util::sync::CancellationToken`.
    #[allow(clippy::default_trait_access)]
    fn test_ctx_without_credstore(section: Option<serde_json::Value>) -> GearCtx {
        GearCtx::new(
            "oagw",
            Uuid::new_v4(),
            Arc::new(FixedConfigProvider { section }),
            Arc::new(ClientHub::default()),
            Default::default(),
        )
    }

    // @cpt-begin:cpt-cf-oagw-dod-gear-registration:p1:inst-gear-reg-return-test-01
    #[tokio::test]
    async fn init_resolves_the_graded_configuration_and_control_plane_state() {
        let section = serde_json::json!({
            "config": {
                "proxy_timeout_secs": 2,
                "allow_http_upstream": true,
                "ssrf_policy": { "enabled": false }
            }
        });
        let ctx = test_ctx(Some(section));
        let gear = OagwGear::default();

        gear.init(&ctx).await.expect("init must succeed");

        let cfg = gear.config().expect("config resolved once at startup");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);

        let state = gear
            .state()
            .expect("control-plane state initialized at startup");
        assert_eq!(state.tenant_count(), 0);

        assert!(
            gear.credstore().is_some(),
            "credstore client resolved once at startup"
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-gear-registration:p1:inst-gear-reg-return-test-01

    #[tokio::test]
    async fn init_falls_back_to_documented_defaults_when_section_is_absent() {
        let ctx = test_ctx(None);
        let gear = OagwGear::default();

        gear.init(&ctx)
            .await
            .expect("init must succeed with no config section");

        let cfg = gear.config().expect("config resolved once at startup");
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
    }

    #[tokio::test]
    async fn init_fails_startup_when_a_key_has_an_invalid_type() {
        let section = serde_json::json!({
            "config": { "proxy_timeout_secs": "not-a-number" }
        });
        let ctx = test_ctx(Some(section));
        let gear = OagwGear::default();

        let result = gear.init(&ctx).await;
        assert!(
            result.is_err(),
            "an invalid key type must fail gear startup"
        );
    }

    // @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-gear-credstore-missing-test-01
    #[tokio::test]
    async fn init_fails_when_no_credstore_client_is_registered() {
        let ctx = test_ctx_without_credstore(None);
        let gear = OagwGear::default();

        let result = gear.init(&ctx).await;
        assert!(
            result.is_err(),
            "init must fail when the credstore dependency is unavailable"
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-gear-credstore-missing-test-01

    #[tokio::test]
    async fn register_rest_requires_init_to_have_run() {
        let ctx = test_ctx(None);
        let gear = OagwGear::default();
        let router = axum::Router::new();
        let openapi = OpenApiRegistryImpl::new();

        let result = gear.register_rest(&ctx, router, &openapi);
        assert!(
            result.is_err(),
            "register_rest must fail before init runs, rather than mount with no state"
        );
    }
}
