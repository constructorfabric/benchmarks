//! Models API and catalog helpers.
//!
//! CONTRACT: `chat_model` and `invalid_model` are used by other services; signatures fixed.

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};

use crate::domain::error::{DomainError, Resource};

/// 400 `invalid_argument`, `field_violations[model].reason = INVALID_MODEL`.
#[must_use]
pub fn invalid_model(model: &str) -> DomainError {
    DomainError::invalid(Resource::Chat, "model", "INVALID_MODEL", format!("model '{model}' is not available"))
}

/// The chat's model in the catalog without the enabled filter (`INVALID_MODEL` when removed).
///
/// # Errors
/// `invalid_model` when the id is not in the catalog.
pub fn chat_model<'a>(snapshot: &'a PolicySnapshot, model_id: &str) -> Result<&'a ModelCatalogEntry, DomainError> {
    snapshot.model(model_id).ok_or_else(|| invalid_model(model_id))
}

/// Models visible to the user: the enabled catalog entries, in catalog order (DESIGN §3.3
/// "Models API", visibility algorithm).
///
/// # Errors
/// Authz errors (`model_access(list)`), policy errors.
pub async fn list_models(
    app: &crate::domain::services::AppServices,
    ctx: &toolkit_security::SecurityContext,
) -> Result<Vec<ModelCatalogEntry>, DomainError> {
    app.authz.model_access(ctx, "list").await?;
    let snapshot = app.policy.current_snapshot(ctx.subject_id()).await?;
    Ok(snapshot.model_catalog.iter().filter(|m| m.enabled).cloned().collect())
}

/// One visible model (404 Model when disabled or unknown).
///
/// # Errors
/// Authz errors (`model_access(read)`), 404 Model, policy errors.
pub async fn get_model(
    app: &crate::domain::services::AppServices,
    ctx: &toolkit_security::SecurityContext,
    model_id: &str,
) -> Result<ModelCatalogEntry, DomainError> {
    app.authz.model_access(ctx, "read").await?;
    let snapshot = app.policy.current_snapshot(ctx.subject_id()).await?;
    snapshot.enabled_model(model_id).cloned().ok_or_else(|| DomainError::not_found(Resource::Model, model_id))
}
