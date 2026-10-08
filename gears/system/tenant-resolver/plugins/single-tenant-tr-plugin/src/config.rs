//! Configuration for the single-tenant resolver plugin.

use serde::Deserialize;

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SingleTenantTrPluginConfig {
    /// Vendor name for GTS instance registration.
    pub vendor: String,

    /// Plugin priority (lower = higher priority).
    /// Set to 1000 so `static_tr_plugin` (priority 100) wins when both are enabled.
    pub priority: i16,
}

impl Default for SingleTenantTrPluginConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 1000,
        }
    }
}
