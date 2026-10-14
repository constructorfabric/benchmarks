//! Model catalog rules the later features consume (DESIGN 4 `Model Catalog Configuration`).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot};

use crate::domain::error::DomainError;

/// The multimodal capability that marks image input support.
const VISION_INPUT: &str = "VISION_INPUT";

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-tier-order
/// The fixed downgrade tier order: `premium`, then `standard`.
pub const DOWNGRADE_TIER_ORDER: [ModelTier; 2] = [ModelTier::Premium, ModelTier::Standard];
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-tier-order

/// The enabled entries of the catalog, in catalog order. An entry that omits `enabled`
/// deserializes as disabled, so it is excluded from the runtime catalog and the Models API.
pub fn enabled_entries(snapshot: &PolicySnapshot) -> impl Iterator<Item = &ModelCatalogEntry> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-enabled-default
    snapshot.model_catalog.iter().filter(|entry| entry.enabled)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-enabled-default
}

/// The default model for new chats: the first enabled entry with `preference.is_default`,
/// else the first enabled entry. The tier is not considered.
#[must_use]
pub fn default_model(snapshot: &PolicySnapshot) -> Option<&ModelCatalogEntry> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-default-model
    enabled_entries(snapshot)
        .find(|entry| {
            entry
                .preference
                .as_ref()
                .is_some_and(|preference| preference.is_default)
        })
        .or_else(|| enabled_entries(snapshot).next())
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-default-model
}

/// Validate a user-selected model: present in the catalog and enabled.
///
/// # Errors
///
/// [`DomainError::InvalidModel`] (`field_violations[model].reason = INVALID_MODEL`).
pub fn validate_selected_model<'a>(
    snapshot: &'a PolicySnapshot,
    model_id: &str,
) -> Result<&'a ModelCatalogEntry, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-selection
    enabled_entries(snapshot)
        .find(|entry| entry.id == model_id)
        .ok_or_else(|| DomainError::InvalidModel {
            model: model_id.to_owned(),
        })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-selection
}

/// The downgrade cascade: the tiers of [`DOWNGRADE_TIER_ORDER`] that have at least one
/// enabled entry.
#[must_use]
pub fn tier_cascade(snapshot: &PolicySnapshot) -> Vec<ModelTier> {
    DOWNGRADE_TIER_ORDER
        .into_iter()
        .filter(|tier| enabled_entries(snapshot).any(|entry| entry.tier == *tier))
        .collect()
}

/// Whether the model accepts image input (`multimodal_capabilities` contains `VISION_INPUT`).
#[must_use]
pub fn supports_images(entry: &ModelCatalogEntry) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-image-capability
    entry
        .multimodal_capabilities
        .iter()
        .any(|capability| capability == VISION_INPUT)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-image-capability
}

/// The six kill switches of the snapshot.
#[must_use]
pub fn kill_switches(snapshot: &PolicySnapshot) -> KillSwitches {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-kill-switches
    snapshot.kill_switches.clone()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-catalog-rules:p1:inst-catalog-kill-switches
}

#[cfg(test)]
#[path = "catalog_rules_tests.rs"]
mod catalog_rules_tests;
