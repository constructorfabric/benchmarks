// Created: 2026-08-29 by Constructor Tech
//! Plugin reference helpers shared between the domain and the registry.

/// `true` when `gts_id`'s instance part equals `plugin_id` (a UUID string).
#[must_use]
pub fn plugin_instance_matches(gts_id: &str, plugin_id: &str) -> bool {
    crate::domain::model::plugin_instance(gts_id) == plugin_id
}
