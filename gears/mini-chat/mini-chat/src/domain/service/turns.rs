//! Turn status and tail-only mutations (retry / edit / delete), DESIGN §3.9.

use crate::infra::db::WriteTransaction;
use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chats::tenant_scope;
use super::stream::{
    MutationMarker, PrepareInput, ReserveTx, StreamStart, find_turn, insert_link,
    insert_running_turn, insert_user_message, validate_content,
};
use super::{Core, now};
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, Resource};
use crate::infra::db::entities::{
    attachment, chat, message, message_attachment, thread_summary, turn,
};
use crate::infra::outbox::QueueKind;

/// Turn status projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Retry,
    Edit,
    Delete,
}

impl MutationKind {
    const fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
            Self::Delete => actions::DELETE_TURN,
        }
    }

    const fn audit_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(
        Resource::Turn,
        "NOT_LATEST_TURN",
        "Only the latest turn can be modified",
    )
}

fn generation_in_progress() -> DomainError {
    DomainError::aborted(
        Resource::Turn,
        "GENERATION_IN_PROGRESS",
        "A generation is already in progress for this chat",
    )
}

/// Latest non-deleted turn by `(started_at, id)`.
///
/// # Errors
/// DB errors.
pub async fn latest_turn(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<turn::Model>, DomainError> {
    Ok(turn::Entity::find()
        .filter(
            Condition::all()
                .add(turn::Column::ChatId.eq(chat_id))
                .add(turn::Column::DeletedAt.is_null()),
        )
        .order_by(turn::Column::StartedAt, Order::Desc)
        .order_by(turn::Column::Id, Order::Desc)
        .limit(1)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(db)
        .await?)
}

struct Preview {
    chat: chat::Model,
    target: turn::Model,
    user_msg: Option<message::Model>,
    attachment_ids: Vec<Uuid>,
    image_file_ids: Vec<String>,
}

impl Core {
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 404 / PEP errors.
    pub async fn turn_status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        let chat = self
            .authorize_chat(ctx, actions::READ_TURN, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let t = find_turn(&conn, chat.tenant_id, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or_else(|| DomainError::not_found(Resource::Turn, &request_id))?;
        let state = match t.state.as_str() {
            "completed" => "done",
            "failed" => "error",
            "cancelled" => "cancelled",
            _ => "running",
        };
        Ok(TurnStatusView {
            request_id: t.request_id,
            state,
            error_code: if state == "error" { t.error_code } else { None },
            assistant_message_id: if matches!(state, "done" | "cancelled") {
                t.assistant_message_id
            } else {
                None
            },
            updated_at: t.updated_at,
        })
    }

    async fn mutation_preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: MutationKind,
    ) -> Result<Preview, DomainError> {
        let scope = authz::chat_scope(&self.enforcer, ctx, kind.action(), Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = super::chats::load_owned_chat(&conn, &scope, ctx, chat_id).await?;
        let target = find_turn(&conn, chat.tenant_id, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Resource::Turn, &request_id))?;
        if target.deleted_at.is_some() {
            return Err(not_latest());
        }
        if target.state == "running" {
            return Err(DomainError::precondition(
                Resource::Turn,
                "turn_state",
                "STATE",
                "the turn is still running",
            ));
        }
        let latest = latest_turn(&conn, chat.tenant_id, chat_id).await?;
        if latest.as_ref().map(|t| t.id) != Some(target.id) {
            return Err(not_latest());
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::authz_denied(Resource::Turn));
        }
        let scope_t = tenant_scope(chat.tenant_id);
        let user_msg = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.eq(request_id))
                    .add(message::Column::Role.eq("user"))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope_t)
            .one(&conn)
            .await?;
        let mut attachment_ids = Vec::new();
        let mut image_file_ids = Vec::new();
        if let Some(um) = &user_msg {
            let links = message_attachment::Entity::find()
                .filter(
                    Condition::all()
                        .add(message_attachment::Column::ChatId.eq(chat_id))
                        .add(message_attachment::Column::MessageId.eq(um.id)),
                )
                .secure()
                .scope_with(&scope_t)
                .all(&conn)
                .await?;
            let mut links = links;
            links.sort_by_key(|l| (l.created_at, l.attachment_id));
            let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
            if !ids.is_empty() {
                let atts = attachment::Entity::find()
                    .filter(
                        Condition::all()
                            .add(attachment::Column::ChatId.eq(chat_id))
                            .add(attachment::Column::Id.is_in(ids.clone()))
                            .add(attachment::Column::DeletedAt.is_null()),
                    )
                    .secure()
                    .scope_with(&scope_t)
                    .all(&conn)
                    .await?;
                for id in ids {
                    if let Some(a) = atts.iter().find(|a| a.id == id) {
                        attachment_ids.push(a.id);
                        if a.attachment_kind == "image"
                            && let Some(f) = &a.provider_file_id
                        {
                            image_file_ids.push(f.clone());
                        }
                    }
                }
            }
        }
        Ok(Preview {
            chat,
            target,
            user_msg,
            attachment_ids,
            image_file_ids,
        })
    }

    /// Retry (`content = None`) or edit (`content = Some`) of the latest turn; streams a new turn.
    ///
    /// # Errors
    /// Pre-stream JSON errors.
    pub async fn mutate_and_stream(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> Result<StreamStart, DomainError> {
        let kind = if new_content.is_some() {
            MutationKind::Edit
        } else {
            MutationKind::Retry
        };
        if let Some(c) = &new_content {
            validate_content(c)?;
        }
        let pv = self
            .mutation_preview(ctx, chat_id, request_id, kind)
            .await?;
        let content = match &new_content {
            Some(c) => c.clone(),
            None => pv
                .user_msg
                .as_ref()
                .map(|m| m.content.clone())
                .unwrap_or_default(),
        };
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        if snapshot.find_model(&pv.chat.model).is_none() {
            return Err(DomainError::invalid_model());
        }
        let web_search = pv.target.web_search_enabled;
        let (decision, attachments) = self
            .preflight_turn(ctx, &pv.chat, &content, pv.image_file_ids.len(), web_search)
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let user_msg_id = Uuid::new_v4();
        let core = Arc::clone(self);
        let tenant = pv.chat.tenant_id;
        let user_id = ctx.subject_id();
        let old = pv.target.clone();
        let old_user_msg = pv.user_msg.clone();
        let att_ids = pv.attachment_ids.clone();
        let content_c = content.clone();
        let decision_c = decision.clone();
        let (user_msg_created, wake) = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let scope = tenant_scope(tenant);
                    let latest = latest_turn(tx, tenant, chat_id).await?;
                    if latest.as_ref().is_some_and(|t| t.state == "running") {
                        return Err(generation_in_progress());
                    }
                    if latest.as_ref().map(|t| t.id) != Some(old.id) {
                        return Err(not_latest());
                    }
                    soft_delete_turn(tx, tenant, chat_id, &old, Some(new_request_id), ts).await?;
                    invalidate_summary(tx, tenant, chat_id, old_user_msg.as_ref()).await?;
                    insert_user_message(
                        tx,
                        tenant,
                        chat_id,
                        user_msg_id,
                        new_request_id,
                        &content_c,
                        ts,
                    )
                    .await?;
                    for a in &att_ids {
                        insert_link(tx, tenant, chat_id, user_msg_id, *a, ts).await?;
                    }
                    let r = ReserveTx {
                        tenant_id: tenant,
                        user_id,
                        chat_id,
                        turn_id,
                        request_id: new_request_id,
                        user_msg_id,
                        content: content_c.clone(),
                        attachment_ids: att_ids.clone(),
                        web_search,
                        decision: decision_c,
                        mutation: Some(MutationMarker),
                    };
                    if let Err(e) = insert_running_turn(tx, &r, None, ts).await {
                        let s = e.to_string();
                        if s.contains("UNIQUE") || s.contains("unique") || s.contains("duplicate") {
                            return Err(generation_in_progress());
                        }
                        return Err(e);
                    }
                    chat::Entity::update_many()
                        .col_expr(chat::Column::UpdatedAt, Expr::value(ts))
                        .filter(chat::Column::Id.eq(chat_id))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let audit = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: kind.audit_type().to_owned(),
                        tenant_id: tenant,
                        actor_user_id: user_id,
                        chat_id,
                        original_request_id: Some(old.request_id),
                        new_request_id: Some(new_request_id),
                        request_id: None,
                        timestamp: ts,
                    });
                    let wake = core
                        .outbox
                        .enqueue(tx, QueueKind::Audit, tenant, &audit)
                        .await?;
                    Ok((ts, wake))
                })
            })
            .await?;
        wake.fire();

        // Setup after the commit: context, provider, reserve (failures mark the new turn failed).
        let mut chat_now = pv.chat.clone();
        chat_now.updated_at = user_msg_created;
        let prep_input = PrepareInput {
            ctx,
            chat: &chat_now,
            content: &content,
            image_file_ids: pv.image_file_ids.clone(),
            web_search,
            request_id: new_request_id,
            exclude_request_from_history: Some(new_request_id),
        };
        let prepared = match self
            .build_turn_request(&prep_input, decision, attachments)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let fail_code = if matches!(
                    &e,
                    DomainError::OutOfRange {
                        reason: "CONTEXT_BUDGET_EXCEEDED",
                        ..
                    }
                ) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                self.fail_unstarted_turn(tenant, turn_id, fail_code).await;
                return Err(e);
            }
        };
        let r = ReserveTx {
            tenant_id: tenant,
            user_id,
            chat_id,
            turn_id,
            request_id: new_request_id,
            user_msg_id,
            content: content.clone(),
            attachment_ids: pv.attachment_ids.clone(),
            web_search,
            decision: prepared.decision.clone(),
            mutation: Some(MutationMarker),
        };
        let core = Arc::clone(self);
        if let Err(e) = self
            .db
            .write_transaction(move |tx| Box::pin(async move { core.reserve_turn_tx(tx, r).await }))
            .await
        {
            let fail_code = if matches!(&e, DomainError::ResourceExhausted { .. }) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted_turn(tenant, turn_id, fail_code).await;
            return Err(e);
        }
        Ok(StreamStart::Live(self.spawn_turn(
            ctx.clone(),
            &chat_now,
            turn_id,
            new_request_id,
            user_msg_id,
            user_msg_created,
            prepared,
        )))
    }

    /// Marks an unstarted retry/edit turn failed (no settlement, no outbox).
    pub async fn fail_unstarted_turn(&self, tenant_id: Uuid, turn_id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let ts = now();
        if let Err(e) = turn::Entity::update_many()
            .col_expr(turn::Column::State, Expr::value("failed"))
            .col_expr(turn::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(turn::Column::CompletedAt, Expr::value(Some(ts)))
            .col_expr(turn::Column::UpdatedAt, Expr::value(ts))
            .filter(
                Condition::all()
                    .add(turn::Column::Id.eq(turn_id))
                    .add(turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .exec(&conn)
            .await
        {
            tracing::debug!(error = %e, turn_id = %turn_id, "mini-chat: marking unstarted turn failed did not persist");
        }
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 400 / 403 / 404 / 409 / PEP errors.
    pub async fn delete_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let pv = self
            .mutation_preview(ctx, chat_id, request_id, MutationKind::Delete)
            .await?;
        let core = Arc::clone(self);
        let tenant = pv.chat.tenant_id;
        let user_id = ctx.subject_id();
        let old = pv.target.clone();
        let old_user_msg = pv.user_msg.clone();
        let wake: Wake = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let latest = latest_turn(tx, tenant, chat_id).await?;
                    if latest.as_ref().map(|t| t.id) != Some(old.id) {
                        return Err(not_latest());
                    }
                    soft_delete_turn(tx, tenant, chat_id, &old, None, ts).await?;
                    invalidate_summary(tx, tenant, chat_id, old_user_msg.as_ref()).await?;
                    let audit = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: MutationKind::Delete.audit_type().to_owned(),
                        tenant_id: tenant,
                        actor_user_id: user_id,
                        chat_id,
                        original_request_id: None,
                        new_request_id: None,
                        request_id: Some(old.request_id),
                        timestamp: ts,
                    });
                    core.outbox
                        .enqueue(tx, QueueKind::Audit, tenant, &audit)
                        .await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

async fn soft_delete_turn(
    tx: &impl DBRunner,
    tenant: Uuid,
    chat_id: Uuid,
    old: &turn::Model,
    replaced_by: Option<Uuid>,
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = tenant_scope(tenant);
    let res = turn::Entity::update_many()
        .col_expr(turn::Column::DeletedAt, Expr::value(Some(ts)))
        .col_expr(turn::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(turn::Column::UpdatedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(turn::Column::Id.eq(old.id))
                .add(turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    if res.rows_affected == 0 {
        return Err(not_latest());
    }
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(ts)))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(old.request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Drops the summary (and clears `is_compressed`) when it covers the mutated turn's user message.
async fn invalidate_summary(
    tx: &impl DBRunner,
    tenant: Uuid,
    chat_id: Uuid,
    user_msg: Option<&message::Model>,
) -> Result<(), DomainError> {
    let scope = tenant_scope(tenant);
    let Some(s) = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?
    else {
        return Ok(());
    };
    let covers = user_msg.is_none_or(|m| {
        (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (m.created_at, m.id)
    });
    if !covers {
        return Ok(());
    }
    thread_summary::Entity::delete_many()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}
