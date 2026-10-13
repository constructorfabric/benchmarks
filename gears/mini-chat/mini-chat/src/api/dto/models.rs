//! Models API DTOs (`ModelDto`, `ModelListDto`, `ModelTierDto`).

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).
// `ModelDto` is only the user-facing projection of a catalog entry: no provider, routing,
// multiplier, `max_output_tokens` or `is_default` fields (DESIGN "Non-Exposure Rules").

/// Response DTO for a single model.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    pub multiplier_display: String,
    // Omitted when the catalog entry has no description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

/// Response DTO for the model list endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

impl From<ModelTier> for ModelTierDto {
    fn from(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Standard => Self::Standard,
            ModelTier::Premium => Self::Premium,
        }
    }
}

impl From<ModelCatalogEntry> for ModelDto {
    fn from(m: ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id,
            display_name: m.display_name,
            tier: m.tier.into(),
            multiplier_display: m.multiplier_display,
            description: (!m.description.is_empty()).then_some(m.description),
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}
