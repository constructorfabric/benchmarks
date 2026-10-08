//! Preflight of a turn (spec §8 steps 6–10): web search kill switch, chat facts
//! (snapshot boundary, prior context tokens, tool facts, images), the quota
//! cascade, the input size limit and the image guards. Nothing is written.

use std::collections::HashMap;

use mini_chat_sdk::UserLimits;
use uuid::Uuid;

use super::StreamService;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::estimated_text_tokens;
use crate::domain::model::{AttachmentKind, AttachmentStatus};
use crate::domain::services::model_catalog::ResolvedModel;
use crate::domain::services::quota::{PreflightDecision, PreflightInput};
use crate::infra::db::entities::chat;
use crate::infra::db::repos::attachment::ChatToolFacts;
use crate::infra::db::repos::message::MessagePosition;
use crate::infra::db::repos::{AttachmentRepo, MessageRepo};

/// Capability enabling image input (DESIGN §2.2 "Model Capability Constraint").
pub const VISION_INPUT: &str = "VISION_INPUT";

/// Input of [`StreamService::preflight`].
#[derive(Debug, Clone, Copy)]
pub struct PreflightRequest<'a> {
    /// The authorized, live chat.
    pub chat: &'a chat::Model,
    pub user_id: Uuid,
    /// The chat's model resolved in the current snapshot.
    pub model: &'a ResolvedModel,
    pub content: &'a str,
    /// Attachments of the message (images among them go to the model).
    pub attachment_ids: &'a [Uuid],
    /// `web_search.enabled`.
    pub web_search: bool,
}

/// Result of a successful preflight.
#[derive(Debug, Clone)]
pub struct Preflighted {
    pub decision: PreflightDecision,
    /// Limits of the decision's policy version (reserve re-check).
    pub user_limits: UserLimits,
    pub facts: ChatToolFacts,
    /// Provider file ids of the ready images among the attachments, in request order.
    pub image_file_ids: Vec<String>,
    /// Provider file id -> Anthropic (secondary) file id of those images that
    /// have an uploaded secondary copy.
    pub secondary_image_ids: HashMap<String, String>,
    /// Latest visible message before the turn (`None` for an empty chat).
    pub boundary: Option<MessagePosition>,
}

impl StreamService {
    /// Steps 6–10 of the send pipeline (also the mutation preflight of retry/edit).
    ///
    /// # Errors
    /// `FeatureDisabled { web_search | images }`, `TooManyImages`,
    /// `QuotaExceeded { scope }`, `InputTooLong`, `VisionNotSupported`, policy and
    /// database failures.
    pub(crate) async fn preflight(&self, r: PreflightRequest<'_>) -> DomainResult<Preflighted> {
        let snapshot = &r.model.snapshot;
        let ks = snapshot.kill_switches;
        let (tenant_id, chat_id) = (r.chat.tenant_id, r.chat.id);

        // 6. Web search kill switch, before the cascade.
        if r.web_search && ks.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                subject: "web_search",
            });
        }

        // 7. Chat facts and the images of the message.
        let conn = self.db.conn()?;
        let boundary = MessageRepo::snapshot_boundary(&conn, tenant_id, chat_id).await?;
        let prior_context_tokens =
            MessageRepo::prior_context_tokens(&conn, tenant_id, chat_id).await?;
        let facts = AttachmentRepo::chat_tool_facts(&conn, tenant_id, chat_id).await?;
        let rows =
            AttachmentRepo::find_in_chat(&conn, tenant_id, chat_id, r.attachment_ids).await?;
        let images: Vec<_> = r
            .attachment_ids
            .iter()
            .filter_map(|id| {
                rows.iter().find(|a| {
                    a.id == *id
                        && a.deleted_at.is_none()
                        && a.attachment_kind == AttachmentKind::Image.as_str()
                })
            })
            .collect();
        let max_images = self.cfg.rag.max_images_per_message;
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        if image_count > max_images {
            return Err(DomainError::TooManyImages { max: max_images });
        }
        let ready_images: Vec<_> = images
            .iter()
            .filter(|a| a.status == AttachmentStatus::Ready.as_str())
            .collect();
        let image_file_ids = ready_images
            .iter()
            .filter_map(|a| a.provider_file_id.clone())
            .collect();
        let secondary_image_ids = ready_images
            .iter()
            .filter(|a| a.secondary_status == "uploaded")
            .filter_map(|a| a.provider_file_id.clone().zip(a.secondary_file_id.clone()))
            .collect();

        // 8. Quota preflight (cascade, daily tool quotas).
        let user_limits = self
            .policy
            .user_limits(r.user_id, snapshot.policy_version)
            .await?;
        let decision = self
            .quota
            .preflight(&PreflightInput {
                tenant_id,
                user_id: r.user_id,
                selected_model_id: r.model.entry.id.clone(),
                snapshot: std::sync::Arc::clone(snapshot),
                user_limits,
                content: r.content.to_owned(),
                image_count,
                prior_context_tokens,
                chat_has_ready_docs: facts.ready_docs && facts.vector_store_id.is_some(),
                chat_has_ready_xlsx: !facts.xlsx_file_ids.is_empty(),
                web_search_requested: r.web_search,
                now: now_utc(),
            })
            .await?;
        let effective = &decision.effective;

        // 9. Input size of the effective model.
        if effective.max_input_tokens > 0
            && estimated_text_tokens(r.content, &effective.estimation_budgets)
                > i64::from(effective.max_input_tokens)
        {
            return Err(DomainError::InputTooLong);
        }

        // 10. Image guards on the effective model.
        if image_count > 0 {
            if ks.disable_images {
                return Err(DomainError::FeatureDisabled { subject: "images" });
            }
            if !effective.has_capability(VISION_INPUT) {
                return Err(DomainError::VisionNotSupported);
            }
        }

        Ok(Preflighted {
            decision,
            user_limits,
            facts,
            image_file_ids,
            secondary_image_ids,
            boundary,
        })
    }
}
