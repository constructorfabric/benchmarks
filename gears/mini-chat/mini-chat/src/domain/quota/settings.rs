//! Quota configuration (`cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration`): the
//! values the quota contracts read from the validated gear configuration.

use toolkit_macros::domain_model;

use crate::config::MiniChatConfig;

/// The configuration values of the quota rules. Validation stays in the configuration
/// algorithm of the foundation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaSettings {
    /// `quota.overshoot_tolerance_factor` (default 1.10, range 1.0 to 1.5).
    pub overshoot_tolerance_factor: f64,
    /// `quota.warning_threshold_pct` (default 80, range 1 to 99).
    pub warning_threshold_pct: u8,
    /// `quota.web_search_daily_quota` (default 75).
    pub web_search_daily_quota: u32,
    /// `quota.code_interpreter_daily_quota` (default 50).
    pub code_interpreter_daily_quota: u32,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor` of the gear configuration (default 50),
    /// never the catalog value.
    pub minimal_generation_floor: u32,
}

impl QuotaSettings {
    /// Read the values from the validated gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-overshoot-warning
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-tool-quotas
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-floor
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-return
        // validation stays in the configuration algorithm of the foundation; the per-message
        // maxima belong to the streaming feature and every other estimation budget comes from
        // the catalog entry, so only the floor is read from `estimation_budgets`
        Self {
            overshoot_tolerance_factor: config.quota.overshoot_tolerance_factor,
            warning_threshold_pct: config.quota.warning_threshold_pct,
            web_search_daily_quota: config.quota.web_search_daily_quota,
            code_interpreter_daily_quota: config.quota.code_interpreter_daily_quota,
            streaming_max_output_tokens: config.streaming.max_output_tokens,
            minimal_generation_floor: config.estimation_budgets.minimal_generation_floor,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-return
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-floor
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-tool-quotas
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-overshoot-warning
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod settings_tests;
