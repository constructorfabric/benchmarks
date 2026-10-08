//! Policy snapshot, kill switches and per-user limits (DESIGN §5.2).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::ModelCatalogEntry;

/// Global emergency flags. Every field is required on deserialize so a renamed or missing
/// switch from a policy plugin fails loudly instead of reading as `false`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned shared policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: i64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Catalog entry by id (enabled or not).
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id)
    }

    /// Enabled catalog entry by id.
    #[must_use]
    pub fn enabled_model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model(id).filter(|m| m.enabled)
    }

    /// Default model for new chats: first enabled `is_default` entry, else the first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Current policy version answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: i64,
    #[serde(default)]
    pub generated_at: Option<String>,
}

/// Per-tier credit limits in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user allocation tied to a policy version. `standard` limits apply to bucket `total`,
/// `premium` limits to bucket `tier:premium`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: i64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status answer (unused by the gear; default plugin body returns inactive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switches_require_every_field() {
        let err = serde_json::from_value::<KillSwitches>(serde_json::json!({"disable_web_search": true}));
        assert!(err.is_err());
    }
}
