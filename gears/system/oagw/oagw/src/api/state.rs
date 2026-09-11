// Created: 2026-09-01 by Constructor Tech
//! Shared state handed to every handler as an `Extension`.

use std::sync::Arc;

use crate::config::OagwConfig;
use crate::domain::store::SharedStore;
use crate::infra::credstore::SecretResolver;
use crate::infra::dp::DataPlane;
use crate::infra::plugin::Registries;
use crate::infra::tenant::TenantChain;

/// Everything a handler needs.
#[derive(Clone)]
pub struct OagwState {
    /// The Control Plane store.
    pub store: SharedStore,
    /// The Data Plane.
    pub dp: Arc<DataPlane>,
    /// Plugin registries.
    pub registries: Registries,
    /// Credential resolution.
    pub secrets: SecretResolver,
    /// Tenant-chain lookup.
    pub tenants: TenantChain,
    /// Gear configuration.
    pub config: Arc<OagwConfig>,
}

impl std::fmt::Debug for OagwState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwState")
            .field("store", &self.store)
            .field("dp", &self.dp)
            .finish()
    }
}
