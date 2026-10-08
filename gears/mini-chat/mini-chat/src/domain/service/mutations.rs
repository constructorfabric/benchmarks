//! Turn mutations: retry / edit / delete of the latest turn (OWNER: streaming core).
//!
//! DESIGN §3.9 "Turn Mutation Rules" and the retry / edit variant of the send sequence:
//! read-only preview → mutation preflight → mutation commit → setup (context, provider,
//! reserve) → stream. Only rejections before the commit leave the previous turn in place.

use std::sync::Arc;
use std::time::Instant;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::{DomainError, reasons, resource_types, stream_codes};
use crate::domain::service::Deps;
use crate::domain::service::billing::build_mutation_audit_event;
use crate::domain::service::chat_access::{AuthorizedChat, load_chat};
use crate::domain::service::finalization::{retry_locked, user_message};
use crate::domain::service::quota::{PreflightDecision, QuotaService};
use crate::domain::service::stream::{
    ImageRef, LaunchSpec, StreamService, StreamStart, empty_content, find_turn, gather,
    link_attachment, new_running_turn, touch_chat,
};
use crate::infra::db::entity::{
    attachment, chat_turn, message, message_attachment, thread_summary,
};

/// Kind of a regenerating mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Regenerate {
    Retry,
    Edit,
}

impl Regenerate {
    const fn action(self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit => actions::EDIT_TURN,
        }
    }

    const fn audit_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
        }
    }
}

fn turn_not_found() -> DomainError {
    DomainError::NotFound {
        resource: resource_types::TURN,
    }
}

fn not_latest() -> DomainError {
    DomainError::aborted(
        reasons::NOT_LATEST_TURN,
        "Only the latest turn can be modified",
    )
}

fn generation_in_progress() -> DomainError {
    DomainError::aborted(
        reasons::GENERATION_IN_PROGRESS,
        "Another generation is already in progress for this chat",
    )
}

/// Latest non-deleted turn of the chat (greatest `(started_at, id)`).
///
/// # Errors
/// Database failure.
pub async fn latest_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Mutation eligibility: terminal (checked first), latest non-deleted, owned by the caller.
///
/// # Errors
/// 400 `STATE`, 409 `NOT_LATEST_TURN`, 403 `AUTHZ_DENIED`.
pub fn check_target(
    target: &chat_turn::Model,
    latest: Option<&chat_turn::Model>,
    user_id: Uuid,
) -> Result<(), DomainError> {
    if target.state == "running" && target.deleted_at.is_none() {
        return Err(DomainError::precondition(
            "turn_state",
            reasons::STATE,
            "The turn is still running",
        ));
    }
    if target.deleted_at.is_some() || latest.map(|l| l.id) != Some(target.id) {
        return Err(not_latest());
    }
    if target.requester_user_id != Some(user_id) {
        return Err(DomainError::authz_denied());
    }
    Ok(())
}

async fn user_message_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::Role.eq("user")),
        )
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Asc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Non-deleted attachments linked to `message_id`, in the stable link order
/// `(created_at, attachment_id)` (retry / edit send the images in the same order).
async fn linked_attachments(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    let links = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.eq(message_id)),
        )
        .secure()
        .scope_with(scope)
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .order_by(message_attachment::Column::AttachmentId, Order::Asc)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let rows = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids.clone()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    Ok(ids
        .iter()
        .filter_map(|id| rows.iter().find(|a| a.id == *id).cloned())
        .collect())
}

/// Summary invalidation: drops the chat's summary when its frontier is at or after `key`
/// (the mutated turn's user message) and clears `is_compressed` on the chat's messages.
///
/// # Errors
/// Database failure.
pub async fn invalidate_summary(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    chat_id: Uuid,
    key: Option<(OffsetDateTime, Uuid)>,
) -> Result<bool, DomainError> {
    let Some(key) = key else { return Ok(false) };
    let row = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(tx)
        .await?;
    let Some(row) = row else { return Ok(false) };
    if (
        row.summarized_up_to_created_at,
        row.summarized_up_to_message_id,
    ) < key
    {
        return Ok(false);
    }
    thread_summary::Entity::delete_many()
        .filter(thread_summary::Column::Id.eq(row.id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(true)
}

async fn soft_delete_turn(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    chat_id: Uuid,
    target: &chat_turn::Model,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let mut upd = chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now));
    if let Some(r) = replaced_by {
        upd = upd.col_expr(chat_turn::Column::ReplacedByRequestId, Expr::value(Some(r)));
    }
    let rows = upd
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(target.id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?
        .rows_affected;
    if rows == 0 {
        return Err(not_latest());
    }
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(target.request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Re-checks eligibility inside the mutation transaction.
async fn recheck(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    user_id: Uuid,
) -> Result<chat_turn::Model, DomainError> {
    let target = find_turn(tx, scope, chat_id, request_id)
        .await?
        .ok_or_else(turn_not_found)?;
    let latest = latest_turn(tx, scope, chat_id).await?;
    check_target(&target, latest.as_ref(), user_id)?;
    Ok(target)
}

struct DeleteCommit {
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    child_scope: AccessScope,
}

async fn delete_in_tx(tx: &DbTx<'_>, deps: &Deps, c: &DeleteCommit) -> Result<Wake, DomainError> {
    let now = OffsetDateTime::now_utc();
    let target = recheck(tx, &c.child_scope, c.chat_id, c.request_id, c.user_id).await?;
    let key = user_message_of(tx, &c.child_scope, c.chat_id, target.request_id)
        .await?
        .map(|m| (m.created_at, m.id));
    soft_delete_turn(tx, &c.child_scope, c.chat_id, &target, None, now).await?;
    invalidate_summary(tx, &c.child_scope, c.chat_id, key).await?;
    let ev = build_mutation_audit_event(
        "turn_delete",
        c.tenant_id,
        c.user_id,
        c.chat_id,
        target.request_id,
        None,
    );
    deps.outbox.enqueue_audit(tx, &ev).await
}

struct RegenerateCommit {
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    scope: AccessScope,
    child_scope: AccessScope,
    old_request_id: Uuid,
    new_request_id: Uuid,
    new_turn_id: Uuid,
    new_message_id: Uuid,
    content: String,
    attachment_ids: Vec<Uuid>,
    web_search_enabled: bool,
    key: Option<(OffsetDateTime, Uuid)>,
    audit_type: &'static str,
}

async fn regenerate_in_tx(
    tx: &DbTx<'_>,
    deps: &Deps,
    c: &RegenerateCommit,
) -> Result<Wake, DomainError> {
    let now = OffsetDateTime::now_utc();
    let target = recheck(tx, &c.child_scope, c.chat_id, c.old_request_id, c.user_id).await?;
    soft_delete_turn(
        tx,
        &c.child_scope,
        c.chat_id,
        &target,
        Some(c.new_request_id),
        now,
    )
    .await?;
    secure_insert::<message::Entity>(
        user_message(
            c.tenant_id,
            c.chat_id,
            c.new_message_id,
            c.new_request_id,
            c.content.clone(),
            now,
        ),
        &c.child_scope,
        tx,
    )
    .await?;
    for id in &c.attachment_ids {
        link_attachment(
            tx,
            &c.child_scope,
            c.tenant_id,
            c.chat_id,
            c.new_message_id,
            *id,
            now,
        )
        .await?;
    }
    secure_insert::<chat_turn::Entity>(
        new_running_turn(
            c.tenant_id,
            c.chat_id,
            c.new_turn_id,
            c.new_request_id,
            c.user_id,
            None,
            c.web_search_enabled,
            now,
        ),
        &c.child_scope,
        tx,
    )
    .await
    .map_err(|e| {
        let e = DomainError::from(e);
        if e.is_unique_violation() {
            generation_in_progress()
        } else {
            e
        }
    })?;
    touch_chat(tx, &c.scope, c.chat_id, now).await?;
    invalidate_summary(tx, &c.child_scope, c.chat_id, c.key).await?;
    let ev = build_mutation_audit_event(
        c.audit_type,
        c.tenant_id,
        c.user_id,
        c.chat_id,
        c.old_request_id,
        Some(c.new_request_id),
    );
    deps.outbox.enqueue_audit(tx, &ev).await
}

/// Writes the preflight columns of a retry/edit turn together with the quota reserve.
async fn reserve_mutation(
    tx: &DbTx<'_>,
    quota: &QuotaService,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    turn_id: Uuid,
    d: &PreflightDecision,
) -> Result<(), DomainError> {
    quota.reserve_in_tx(tx, tenant_id, user_id, d).await?;
    let rows = chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::ReserveTokens,
            Expr::value(Some(d.reserve_tokens)),
        )
        .col_expr(
            chat_turn::Column::MaxOutputTokensApplied,
            Expr::value(Some(d.max_output_tokens_applied)),
        )
        .col_expr(
            chat_turn::Column::ReservedCreditsMicro,
            Expr::value(Some(d.reserved_credits_micro)),
        )
        .col_expr(
            chat_turn::Column::PolicyVersionApplied,
            Expr::value(Some(i64::try_from(d.policy_version).unwrap_or(i64::MAX))),
        )
        .col_expr(
            chat_turn::Column::EffectiveModel,
            Expr::value(Some(d.effective.id.clone())),
        )
        .col_expr(
            chat_turn::Column::MinimalGenerationFloorApplied,
            Expr::value(Some(d.minimal_generation_floor_applied)),
        )
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(OffsetDateTime::now_utc()))
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn_id))
                .add(chat_turn::Column::State.eq("running"))
                .add(chat_turn::Column::ReserveTokens.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await?
        .rows_affected;
    if rows == 0 {
        return Err(DomainError::internal(
            "retry/edit turn is no longer running",
        ));
    }
    Ok(())
}

/// Error code of a retry/edit setup failure after the mutation commit.
fn setup_failure_code(e: &DomainError) -> &'static str {
    match e {
        DomainError::OutOfRange { reason, .. } if reason == reasons::CONTEXT_BUDGET_EXCEEDED => {
            stream_codes::CONTEXT_LENGTH_EXCEEDED
        }
        DomainError::ResourceExhausted { .. } => stream_codes::QUOTA_EXCEEDED,
        _ => stream_codes::TURN_SETUP_FAILED,
    }
}

pub struct MutationService {
    deps: Arc<Deps>,
    quota: Arc<QuotaService>,
    stream: Arc<StreamService>,
}

impl MutationService {
    #[must_use]
    pub fn new(deps: Arc<Deps>, quota: Arc<QuotaService>, stream: Arc<StreamService>) -> Self {
        Self {
            deps,
            quota,
            stream,
        }
    }

    /// Read-only preview: chat (404), turn (404), terminal / latest / owner checks.
    async fn preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        action: &str,
    ) -> Result<(AuthorizedChat, chat_turn::Model), DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, action).await?;
        let conn = self.deps.db.conn()?;
        let target = find_turn(&conn, &ac.child_scope, ac.chat.id, request_id)
            .await?
            .ok_or_else(turn_not_found)?;
        let latest = latest_turn(&conn, &ac.child_scope, ac.chat.id).await?;
        check_target(&target, latest.as_ref(), ctx.subject_id())?;
        Ok((ac, target))
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 403/503/404, 400 `STATE`, 409 `NOT_LATEST_TURN`, 500.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let (ac, _) = self
            .preview(ctx, chat_id, request_id, actions::DELETE_TURN)
            .await?;
        let c = DeleteCommit {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id: ac.chat.id,
            request_id,
            child_scope: ac.child_scope,
        };
        let c = Arc::new(c);
        let wake = retry_locked(|| {
            let deps = Arc::clone(&self.deps);
            let c = Arc::clone(&c);
            self.deps
                .db
                .transaction(move |tx| Box::pin(async move { delete_in_tx(tx, &deps, &c).await }))
        })
        .await
        .map_err(internal_payload)?;
        wake.fire();
        Ok(())
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    ///
    /// # Errors
    /// As for `messages:stream`, plus the mutation rules.
    pub async fn retry(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<StreamStart, DomainError> {
        self.regenerate(ctx, chat_id, request_id, None, Regenerate::Retry)
            .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// As for `messages:stream`, plus the mutation rules and `EMPTY_CONTENT`.
    pub async fn edit(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<StreamStart, DomainError> {
        self.regenerate(ctx, chat_id, request_id, Some(content), Regenerate::Edit)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn regenerate(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
        kind: Regenerate,
    ) -> Result<StreamStart, DomainError> {
        let started = Instant::now();
        let deps = &self.deps;
        let (ac, target) = self
            .preview(ctx, chat_id, request_id, kind.action())
            .await?;
        if new_content.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(empty_content());
        }
        let AuthorizedChat {
            chat,
            scope,
            child_scope,
        } = ac;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();

        // Mutation preflight (no writes).
        let (snapshot, _) = deps
            .policy
            .resolve_model(user_id, &chat.model, false)
            .await?;
        let (orig, attachments, gathered) = {
            let conn = deps.db.conn()?;
            let orig = user_message_of(&conn, &child_scope, chat.id, target.request_id)
                .await?
                .ok_or_else(|| DomainError::internal("turn without user message"))?;
            let attachments = linked_attachments(&conn, &child_scope, chat.id, orig.id).await?;
            let gathered = gather(&conn, &child_scope, chat.id, Some(target.request_id)).await?;
            (orig, attachments, gathered)
        };
        let content = new_content.unwrap_or_else(|| orig.content.clone());
        let web_search = target.web_search_enabled;
        if web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }
        let images: Vec<ImageRef> = attachments
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .map(ImageRef::from)
            .collect();
        let decision = self
            .stream
            .preflight_checks(
                tenant_id, user_id, &chat, &content, &images, web_search, gathered,
            )
            .await?;

        // Mutation commit.
        let c = RegenerateCommit {
            tenant_id,
            user_id,
            chat_id: chat.id,
            scope,
            child_scope: child_scope.clone(),
            old_request_id: target.request_id,
            new_request_id: Uuid::new_v4(),
            new_turn_id: Uuid::new_v4(),
            new_message_id: Uuid::new_v4(),
            content: content.clone(),
            attachment_ids: attachments.iter().map(|a| a.id).collect(),
            web_search_enabled: web_search,
            key: Some((orig.created_at, orig.id)),
            audit_type: kind.audit_type(),
        };
        let (new_request_id, new_turn_id) = (c.new_request_id, c.new_turn_id);
        let c = Arc::new(c);
        let wake = retry_locked(|| {
            let deps2 = Arc::clone(deps);
            let c = Arc::clone(&c);
            deps.db.transaction(move |tx| {
                Box::pin(async move { regenerate_in_tx(tx, &deps2, &c).await })
            })
        })
        .await
        .map_err(|e| {
            if e.is_unique_violation() {
                generation_in_progress()
            } else {
                internal_payload(e)
            }
        })?;
        wake.fire();

        // Setup after the commit; a failure marks the new turn failed.
        let setup = async {
            let planned = self
                .stream
                .plan_turn(
                    tenant_id,
                    user_id,
                    &chat,
                    &child_scope,
                    new_request_id,
                    &content,
                    &images,
                    &decision,
                )
                .await?;
            retry_locked(|| {
                let quota = Arc::clone(&self.quota);
                let scope = child_scope.clone();
                let d = decision.clone();
                deps.db.transaction(move |tx| {
                    Box::pin(async move {
                        reserve_mutation(tx, &quota, &scope, tenant_id, user_id, new_turn_id, &d)
                            .await
                    })
                })
            })
            .await?;
            Ok::<_, DomainError>(planned)
        }
        .await;
        let planned = match setup {
            Ok(p) => p,
            Err(e) => {
                let code = setup_failure_code(&e);
                if let Err(e2) = self.fail_unstarted(&child_scope, new_turn_id, code).await {
                    tracing::error!(turn_id = %new_turn_id, error = %e2, "could not fail retry/edit turn");
                }
                return Err(e);
            }
        };

        Ok(StreamStart::Live(self.stream.launch(LaunchSpec {
            tenant_id,
            user_id,
            chat_id: chat.id,
            selected_model: chat.model.clone(),
            turn_id: new_turn_id,
            request_id: new_request_id,
            decision,
            planned,
            started,
        })))
    }

    /// Plain CAS of an unstarted retry/edit turn to `failed` (no settlement, no events).
    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        code: &str,
    ) -> Result<(), DomainError> {
        let now = OffsetDateTime::now_utc();
        let conn = self.deps.db.conn()?;
        chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::State, Expr::value("failed"))
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(Some(code.to_owned())),
            )
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(())
    }
}

/// The outbox payload-size error of a mutation is a 500 (not the chat-delete 400).
fn internal_payload(e: DomainError) -> DomainError {
    match e {
        DomainError::InvalidFormat { message, .. } => DomainError::internal(message),
        other => other,
    }
}

#[cfg(test)]
#[path = "mutations_tests.rs"]
mod mutations_tests;
