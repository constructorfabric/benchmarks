//! Policy contract (DESIGN 5.2.1 / 5.2.6 and Appendix A): snapshot, kill
//! switches and per-user limits.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::ModelCatalogEntry;

/// Emergency switches of a policy snapshot. Every field is required so a
/// snapshot from a plugin with a missing or renamed switch fails to
/// deserialize instead of reading the switch as `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the policy wire format"
)]
pub struct KillSwitches {
    /// Premium tier is unavailable.
    pub disable_premium_tier: bool,
    /// Every turn runs on the standard tier.
    pub force_standard_tier: bool,
    /// `web_search` is never sent.
    pub disable_web_search: bool,
    /// `file_search` is never sent.
    pub disable_file_search: bool,
    /// Image inputs are rejected.
    pub disable_images: bool,
    /// `code_interpreter` is never sent.
    pub disable_code_interpreter: bool,
}

/// Immutable policy snapshot of one policy version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    /// Monotonic policy version.
    pub policy_version: u64,
    /// Model catalog (disabled entries included).
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Kill switches.
    pub kill_switches: KillSwitches,
}

/// Answer of `get_current_policy_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    /// Current policy version.
    pub policy_version: u64,
    /// When the version was generated.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub generated_at: Option<OffsetDateTime>,
}

/// Credit limits of one tier (micro-credits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    /// Daily limit.
    pub limit_daily_credits_micro: u64,
    /// Monthly limit.
    pub limit_monthly_credits_micro: u64,
}

/// Per-user allocation: `standard` is the overall cap (bucket `total`),
/// `premium` the premium sub-cap (bucket `tier:premium`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// User the limits belong to.
    pub user_id: Uuid,
    /// Policy version the limits were computed for.
    pub policy_version: u64,
    /// Overall cap.
    pub standard: TierLimits,
    /// Premium sub-cap.
    pub premium: TierLimits,
}

/// Answer of `check_user_license`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    /// The user holds an active license.
    pub active: bool,
}
