//! Shared handler state.

use std::sync::Arc;

use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::plugin::token_cache::TokenCacheConfig;
use crate::infra::proxy::ProxyService;

/// Everything the REST handlers need, injected as an axum `Extension`.
#[derive(Clone)]
pub struct OagwState {
    /// Control plane (upstreams, routes, plugins).
    pub control_plane: Arc<ControlPlaneService>,
    /// Data plane that serves `/oagw/v1/proxy/{alias}`.
    pub proxy: Arc<ProxyService>,
    /// Effective gear configuration.
    pub config: Arc<OagwConfig>,
}

impl OagwState {
    /// Build the state from its parts.
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        proxy: Arc<ProxyService>,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            control_plane,
            proxy,
            config,
        }
    }

    /// Assemble the whole state — both planes and the plugin registries — from
    /// a control plane, a credential resolver and the gear configuration.
    #[must_use]
    pub fn assemble(
        control_plane: Arc<ControlPlaneService>,
        resolver: &crate::infra::credentials::SecretResolver,
        config: Arc<OagwConfig>,
    ) -> Self {
        let cache = TokenCacheConfig::from_gear_config(&config);
        let registries = PluginRegistries::with_builtins(resolver, &cache);
        let data_plane = Arc::new(control_plane.data_plane(None));
        Self {
            control_plane,
            proxy: Arc::new(ProxyService::new(
                data_plane,
                registries,
                Arc::clone(&config),
            )),
            config,
        }
    }
}
