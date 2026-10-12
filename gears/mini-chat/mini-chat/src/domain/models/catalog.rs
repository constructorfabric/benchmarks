//! Rules over the model catalog of a policy snapshot.

use mini_chat_sdk::ModelCatalogEntry;

use crate::domain::error::DomainError;

/// The globally enabled entries, in catalog order.
pub fn enabled_models(catalog: &[ModelCatalogEntry]) -> impl Iterator<Item = &ModelCatalogEntry> {
    catalog.iter().filter(|entry| entry.enabled)
}

/// The enabled entry with the given id.
#[must_use]
pub fn find_enabled_model<'a>(
    catalog: &'a [ModelCatalogEntry],
    model_id: &str,
) -> Option<&'a ModelCatalogEntry> {
    enabled_models(catalog).find(|entry| entry.id == model_id)
}

/// The default model for new chats: the first enabled entry flagged `is_default`, else the first
/// enabled entry. The tier is not considered.
#[must_use]
pub fn select_default_model(catalog: &[ModelCatalogEntry]) -> Option<&ModelCatalogEntry> {
    enabled_models(catalog)
        .find(|entry| entry.is_default())
        .or_else(|| enabled_models(catalog).next())
}

/// Resolves the model of a new chat: the requested one (it must be an enabled catalog entry) or
/// the default model.
///
/// # Errors
/// `InvalidModel` when the requested model is unknown or disabled, or when no model is enabled.
pub fn resolve_chat_model(
    catalog: &[ModelCatalogEntry],
    requested: Option<&str>,
) -> Result<String, DomainError> {
    match requested {
        Some(model_id) => find_enabled_model(catalog, model_id)
            .map(|entry| entry.id.clone())
            .ok_or_else(|| DomainError::InvalidModel {
                detail: format!("model `{model_id}` is not an enabled model"),
            }),
        None => select_default_model(catalog)
            .map(|entry| entry.id.clone())
            .ok_or_else(|| DomainError::InvalidModel {
                detail: "no model is enabled".to_owned(),
            }),
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
