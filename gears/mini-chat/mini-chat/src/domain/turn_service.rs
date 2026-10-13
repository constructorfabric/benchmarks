//! Turn mutations: retry, edit and delete of the latest turn (DESIGN §3.9).

use std::sync::Arc;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::quota::reserve_and_recheck;
use crate::domain::service::MiniChat;
use crate::domain::stream::StreamStart;
use crate::domain::stream::send::{find_turn, insert_user_message, reserve_of, touch_chat};
use crate::infra::db::entity::{attachments, chat_turns, chats, message_attachments, messages, thread_summaries};
use crate::infra::db::now;
use crate::infra::outbox::{OutboxEnqueuer, Queue};

/// Kind of mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Retry,
    Edit(String),
}

/// Validated mutation target.
#[derive(Debug, Clone)]
pub struct MutationTarget {
    pub chat: chats::Model,
    pub turn: chat_turns::Model,
    pub user_message: messages::Model,
    pub attachments: Vec<attachments::Model>,
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Latest non-deleted turn of a chat by `(started_at, id)`.
///
/// # Errors
/// Database failure.
pub async fn latest_turn(runner: &impl DBRunner, tenant: Uuid, chat_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .order_by_desc(chat_turns::Column::StartedAt)
        .order_by_desc(chat_turns::Column::Id)
        .limit(1)
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .one(runner)
        .await?)
}

/// Soft-delete `turn` (CAS on not deleted, not running) and its messages; drop a
/// summary that covers its user message. `Err(NotLatestTurn)` when the CAS fails.
async fn soft_delete_turn(
    tx: &impl DBRunner,
    chat: &chats::Model,
    turn: &chat_turns::Model,
    user_message: &messages::Model,
    replaced_by: Option<Uuid>,
    ts: time::OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = AccessScope::for_tenant(chat.tenant_id);
    let latest = latest_turn(tx, chat.tenant_id, chat.id).await?;
    if latest.as_ref().map(|t| t.id) != Some(turn.id) {
        return Err(DomainError::NotLatestTurn);
    }
    let res = chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(ts)))
        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn.id))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(chat_turns::Column::State.ne("running")),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    if res.rows_affected == 0 {
        return Err(DomainError::NotLatestTurn);
    }
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(ts)))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat.id))
                .add(messages::Column::RequestId.eq(turn.request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    invalidate_summary(tx, chat, user_message).await
}

/// Delete the chat summary when its frontier covers `user_message`.
async fn invalidate_summary(tx: &impl DBRunner, chat: &chats::Model, user_message: &messages::Model) -> Result<(), DomainError> {
    let scope = AccessScope::for_tenant(chat.tenant_id);
    let Some(s) = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat.id))
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?
    else {
        return Ok(());
    };
    let covers = (s.summarized_up_to_created_at, s.summarized_up_to_message_id)
        >= (user_message.created_at, user_message.id);
    if !covers {
        return Ok(());
    }
    thread_summaries::Entity::delete_many()
        .filter(thread_summaries::Column::ChatId.eq(chat.id))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(false))
        .filter(messages::Column::ChatId.eq(chat.id))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}

async fn enqueue_mutation_audit(
    tx: &impl DBRunner,
    outbox: &OutboxEnqueuer,
    event: TurnMutationAuditEvent,
) -> Result<Wake, DomainError> {
    let tenant = event.tenant_id;
    outbox
        .enqueue(tx, Queue::Audit, tenant, &MiniChatAuditEvent::Mutation(event))
        .await
}

impl MiniChat {
    /// Read-only mutation preview (latest turn, terminal state, ownership).
    ///
    /// # Errors
    /// 404 / 400 `turn_state` / 409 `NOT_LATEST_TURN` / 403.
    pub async fn mutation_preview(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<MutationTarget, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let turn = find_turn(&conn, chat.tenant_id, chat.id, request_id)
            .await?
            .ok_or_else(|| DomainError::TurnNotFound(request_id.to_string()))?;
        if turn.deleted_at.is_none() && turn.state == "running" {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = latest_turn(&conn, chat.tenant_id, chat.id).await?;
        if turn.deleted_at.is_some() || latest.as_ref().map(|t| t.id) != Some(turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AuthzDenied);
        }
        let scope = AccessScope::for_tenant(chat.tenant_id);
        let user_message = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat.id))
                    .add(messages::Column::RequestId.eq(turn.request_id))
                    .add(messages::Column::Role.eq("user"))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::Internal("turn without user message".into()))?;
        let links = message_attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat.id))
                    .add(message_attachments::Column::MessageId.eq(user_message.id)),
            )
            .order_by_asc(message_attachments::Column::CreatedAt)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        let mut atts = Vec::new();
        if !links.is_empty() {
            let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
            let rows = attachments::Entity::find()
                .filter(
                    Condition::all()
                        .add(attachments::Column::ChatId.eq(chat.id))
                        .add(attachments::Column::Id.is_in(ids.clone()))
                        .add(attachments::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .all(&conn)
                .await?;
            for id in ids {
                if let Some(a) = rows.iter().find(|r| r.id == id) {
                    atts.push(a.clone());
                }
            }
        }
        Ok(MutationTarget {
            chat,
            turn,
            user_message,
            attachments: atts,
        })
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// As for the preview, plus 500 on outbox failure.
    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<(), DomainError> {
        let target = self
            .mutation_preview(ctx, actions::DELETE_TURN, chat_id, request_id)
            .await?;
        let outbox = self.outbox.clone();
        let actor = ctx.subject_id();
        let wake = crate::infra::db::tx_retry(&self.db, move |tx| {
                let outbox = outbox.clone();
                let target = target.clone();
                Box::pin(async move {
                    let ts = now();
                    soft_delete_turn(tx, &target.chat, &target.turn, &target.user_message, None, ts).await?;
                    enqueue_mutation_audit(
                        tx,
                        &outbox,
                        TurnMutationAuditEvent {
                            event_type: "turn_delete".into(),
                            tenant_id: target.chat.tenant_id,
                            actor_user_id: actor,
                            chat_id: target.chat.id,
                            original_request_id: None,
                            new_request_id: None,
                            request_id: Some(target.turn.request_id),
                            timestamp: rfc3339(ts),
                        },
                    )
                    .await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// Retry or edit the latest turn and stream the new answer.
    ///
    /// # Errors
    /// Preview / preflight rejections leave the previous turn unchanged;
    /// setup failures after the commit mark the new turn `failed`.
    #[allow(clippy::too_many_lines)]
    pub async fn start_mutation(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        mutation: Mutation,
    ) -> Result<StreamStart, DomainError> {
        if let Mutation::Edit(content) = &mutation
            && content.trim().is_empty()
        {
            return Err(DomainError::EmptyContent);
        }
        let action = match mutation {
            Mutation::Retry => actions::RETRY_TURN,
            Mutation::Edit(_) => actions::EDIT_TURN,
        };
        let target = self.mutation_preview(ctx, action, chat_id, request_id).await?;
        let text = match &mutation {
            Mutation::Retry => target.user_message.content.clone(),
            Mutation::Edit(c) => c.clone(),
        };
        let images: Vec<attachments::Model> = target
            .attachments
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .cloned()
            .collect();
        if images.len() as u64 > u64::from(self.cfg.rag.max_images_per_message) {
            return Err(DomainError::TooManyImages(format!(
                "at most {} images per message",
                self.cfg.rag.max_images_per_message
            )));
        }
        let plan = self
            .quota_preflight(ctx, &target.chat, &text, images.len(), target.turn.web_search_enabled)
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let outbox = self.outbox.clone();
        let actor = ctx.subject_id();
        let event_type = match mutation {
            Mutation::Retry => "turn_retry",
            Mutation::Edit(_) => "turn_edit",
        };
        let t2 = target.clone();
        let text2 = text.clone();
        let commit = crate::infra::db::tx_retry(&self.db, move |tx| {
                let outbox = outbox.clone();
                let t2 = t2.clone();
                let text2 = text2.clone();
                Box::pin(async move {
                    let ts = now();
                    soft_delete_turn(tx, &t2.chat, &t2.turn, &t2.user_message, Some(new_request_id), ts).await?;
                    let att_ids: Vec<Uuid> = t2.attachments.iter().map(|a| a.id).collect();
                    insert_user_message(tx, &t2.chat, new_request_id, &text2, &att_ids, ts).await?;
                    let turn = chat_turns::ActiveModel {
                        id: Set(new_turn_id),
                        tenant_id: Set(t2.chat.tenant_id),
                        chat_id: Set(t2.chat.id),
                        request_id: Set(new_request_id),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(actor)),
                        state: Set("running".into()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        error_detail: Set(None),
                        reserve_tokens: Set(None),
                        max_output_tokens_applied: Set(None),
                        reserved_credits_micro: Set(None),
                        policy_version_applied: Set(None),
                        effective_model: Set(None),
                        minimal_generation_floor_applied: Set(None),
                        deleted_at: Set(None),
                        replaced_by_request_id: Set(None),
                        started_at: Set(ts),
                        last_progress_at: Set(Some(ts)),
                        web_search_enabled: Set(t2.turn.web_search_enabled),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(ts),
                    };
                    chat_turns::Entity::insert(turn)
                        .secure()
                        .scope_unchecked(&AccessScope::for_tenant(t2.chat.tenant_id))?
                        .exec(tx)
                        .await?;
                    touch_chat(tx, &t2.chat, ts).await?;
                    enqueue_mutation_audit(
                        tx,
                        &outbox,
                        TurnMutationAuditEvent {
                            event_type: event_type.into(),
                            tenant_id: t2.chat.tenant_id,
                            actor_user_id: actor,
                            chat_id: t2.chat.id,
                            original_request_id: Some(t2.turn.request_id),
                            new_request_id: Some(new_request_id),
                            request_id: None,
                            timestamp: rfc3339(ts),
                        },
                    )
                    .await
                })
            })
            .await;
        let wake = match commit {
            Ok(w) => w,
            Err(e) if e.is_unique_violation() => return Err(DomainError::GenerationInProgress),
            Err(e) => return Err(e),
        };
        wake.fire();

        // Post-commit setup: context, provider, reserve (last step).
        let setup = async {
            let rplan = self
                .build_request(ctx, &target.chat, &plan, &text, &images, Some(new_request_id))
                .await?;
            let reserve = reserve_of(&plan);
            let floor = plan.floor_applied;
            let eff_model = plan.decision.effective.id.clone();
            let premium = plan.decision.tier == mini_chat_sdk::ModelTier::Premium;
            let periods = plan.periods;
            let limits = plan.limits.clone();
            let policy_version = plan.snapshot.policy_version;
            let tenant = target.chat.tenant_id;
            crate::infra::db::tx_retry(&self.db, move |tx| {
                    let eff_model = eff_model.clone();
                    let limits = limits.clone();
                    Box::pin(async move {
                        reserve_and_recheck(tx, tenant, actor, periods, premium, reserve.reserved_credits_micro, &limits)
                            .await?;
                        let res = chat_turns::Entity::update_many()
                            .col_expr(chat_turns::Column::ReserveTokens, Expr::value(Some(reserve.reserve_tokens)))
                            .col_expr(
                                chat_turns::Column::MaxOutputTokensApplied,
                                Expr::value(Some(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                            )
                            .col_expr(
                                chat_turns::Column::ReservedCreditsMicro,
                                Expr::value(Some(reserve.reserved_credits_micro)),
                            )
                            .col_expr(
                                chat_turns::Column::PolicyVersionApplied,
                                Expr::value(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                            )
                            .col_expr(chat_turns::Column::EffectiveModel, Expr::value(Some(eff_model)))
                            .col_expr(
                                chat_turns::Column::MinimalGenerationFloorApplied,
                                Expr::value(Some(i32::try_from(floor).unwrap_or(i32::MAX))),
                            )
                            .filter(
                                Condition::all()
                                    .add(chat_turns::Column::Id.eq(new_turn_id))
                                    .add(chat_turns::Column::State.eq("running")),
                            )
                            .secure()
                            .scope_with(&AccessScope::for_tenant(tenant))
                            .exec(tx)
                            .await?;
                        if res.rows_affected == 0 {
                            return Err(DomainError::Internal("mutation turn is no longer running".into()));
                        }
                        Ok(())
                    })
                })
                .await?;
            Ok::<_, DomainError>(rplan)
        }
        .await;
        match setup {
            Ok(rplan) => {
                let summary_applied = rplan.summary_token_estimate;
                let assistant_message_id = Uuid::new_v4();
                let run = self.turn_run(ctx, &target.chat, new_turn_id, new_request_id, assistant_message_id, &plan, rplan);
                Ok(StreamStart::Live(self.launch(run, summary_applied)))
            }
            Err(e) => {
                let code = match &e {
                    DomainError::ContextBudgetExceeded(_) => "context_length_exceeded",
                    DomainError::QuotaExceeded(_) => "quota_exceeded",
                    _ => "turn_setup_failed",
                };
                self.fail_unstarted_turn(target.chat.tenant_id, new_turn_id, code, &e.to_string())
                    .await;
                Err(e)
            }
        }
    }

    /// Plain CAS of an unstarted retry/edit turn to `failed` (no settlement, no outbox).
    async fn fail_unstarted_turn(&self, tenant: Uuid, turn_id: Uuid, code: &str, detail: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let ts = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value("failed"))
            .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(chat_turns::Column::ErrorDetail, Expr::value(Some(detail.to_owned())))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
            .filter(
                Condition::all()
                    .add(chat_turns::Column::Id.eq(turn_id))
                    .add(chat_turns::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to mark unstarted turn failed");
        }
    }
}
