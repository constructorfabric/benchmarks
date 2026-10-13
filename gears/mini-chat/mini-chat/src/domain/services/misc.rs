//! Models API, reactions and quota status.

use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::quota::TierStatus;
use crate::infra::db::entities::message_reactions;
use crate::infra::db::repo;

/// Quota status response.
#[derive(Debug, Clone)]
pub struct QuotaStatusView {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

impl MiniChatService {
    /// Enabled catalog models.
    ///
    /// # Errors
    /// Authorization / plugin errors.
    pub async fn list_models(self: &Arc<Self>, ctx: &SecurityContext) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.model_access(ctx, actions::LIST).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snapshot.model_catalog.into_iter().filter(|m| m.enabled).collect())
    }

    /// One enabled model (404 when missing or disabled).
    ///
    /// # Errors
    /// `ModelNotFound`, authorization / plugin errors.
    pub async fn get_model(self: &Arc<Self>, ctx: &SecurityContext, id: &str) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.model_access(ctx, actions::READ).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        snapshot
            .model_catalog
            .into_iter()
            .find(|m| m.id == id && m.enabled)
            .ok_or(DomainError::ModelNotFound)
    }

    /// Quota status of the caller.
    ///
    /// # Errors
    /// Authorization / plugin / database errors.
    pub async fn quota_status(self: &Arc<Self>, ctx: &SecurityContext) -> Result<QuotaStatusView, DomainError> {
        self.authz.quota_scope(ctx).await?;
        let user = ctx.subject_id();
        let version = self.policy.current_snapshot(user).await?.policy_version;
        let limits = self.policy.user_limits(user, version).await?;
        let conn = self.db.conn()?;
        let tiers = self
            .quota
            .status(&conn, ctx.subject_tenant_id(), user, &limits, clock::now())
            .await?;
        Ok(QuotaStatusView {
            tiers,
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        })
    }

    async fn reaction_target(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Uuid, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = repo::messages::find_in_chat(&conn, chat.tenant_id, chat.id, message_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok(chat.tenant_id)
    }

    /// Sets (upserts) the caller's reaction on an assistant message.
    ///
    /// # Errors
    /// `InvalidReaction`, 404, `ReactionTargetNotAssistant`.
    pub async fn set_reaction(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<message_reactions::Model, DomainError> {
        if reaction != "like" && reaction != "dislike" {
            return Err(DomainError::InvalidReaction);
        }
        let tenant = self
            .reaction_target(ctx, actions::SET_REACTION, chat_id, message_id)
            .await?;
        let conn = self.db.conn()?;
        repo::reactions::upsert(&conn, tenant, ctx.subject_id(), message_id, reaction, clock::now()).await
    }

    /// Removes the caller's reaction (idempotent).
    ///
    /// # Errors
    /// 404, `ReactionTargetNotAssistant`.
    pub async fn delete_reaction(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let tenant = self
            .reaction_target(ctx, actions::DELETE_REACTION, chat_id, message_id)
            .await?;
        let conn = self.db.conn()?;
        repo::reactions::delete(&conn, tenant, ctx.subject_id(), message_id).await
    }
}
