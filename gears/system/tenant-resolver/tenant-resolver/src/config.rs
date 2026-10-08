//! Configuration for the tenant resolver gear.

use serde::Deserialize;

/// Gear configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantResolverConfig {
    /// Vendor selector used to pick a plugin implementation.
    ///
    /// The gear queries types-registry for plugin instances matching
    /// this vendor and selects the one with lowest priority.
    pub vendor: String,
}

impl Default for TenantResolverConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
        }
    }
}
