//! Model visibility and the public projection (DESIGN 3.3 `Models API`).

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot};
use toolkit_macros::domain_model;

use crate::domain::catalog_rules::enabled_entries;

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-non-exposure
/// The seven public fields of a visible model. It carries no provider, routing,
/// multiplier, version, token-limit or default-flag data.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleModel {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTier,
    pub multiplier_display: String,
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-non-exposure

/// Project one catalog entry to its seven public fields.
fn project(entry: &ModelCatalogEntry) -> VisibleModel {
    VisibleModel {
        model_id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        tier: entry.tier,
        multiplier_display: entry.multiplier_display.clone(),
        description: entry.description.clone(),
        multimodal_capabilities: entry.multimodal_capabilities.clone(),
        context_window: entry.context_window,
    }
}

/// The globally enabled catalog entries, projected, in catalog order.
#[must_use]
pub fn visible_models(snapshot: &PolicySnapshot) -> Vec<VisibleModel> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-filter
    let visible = enabled_entries(snapshot);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-filter
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-project
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-list
    visible.map(project).collect()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-list
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-project
}

/// The visible model with the given id, or `None` when it is disabled or not in the catalog.
#[must_use]
pub fn find_visible_model(snapshot: &PolicySnapshot, model_id: &str) -> Option<VisibleModel> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-single
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-find
    let found = enabled_entries(snapshot).find(|entry| entry.id == model_id);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-find
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-not-found
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-not-found
    // a disabled model and an unknown model are the same `None`, mapped to 404 by the caller
    let entry = found?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-not-found
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-not-found
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-single
    Some(project(entry))
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-return-single
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-model-visibility:p1:inst-visibility-single
}

#[cfg(test)]
#[path = "model_visibility_tests.rs"]
mod model_visibility_tests;
