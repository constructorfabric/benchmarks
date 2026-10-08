//! Turn status and turn mutations: retry, edit and delete of the latest
//! turn (D "Turn Status API", D§3.9 "Turn Mutation Rules", S§6.5).
//!
//! Retry / edit: read-only preview (one PDP call) → mutation preflight of
//! the stream service → mutation commit (one transaction) → setup and
//! provider task of the new turn through the stream service, the same
//! reserve / finalization / outbox path as a send.

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent, TurnMutationAuditEventType};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz;
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::models::{TurnStatusState, TurnStatusView};
use crate::domain::ports::{OutboxPort, PendingWakes};
use crate::domain::services::stream_service::{
    LiveTurn, NewTurn, TurnInput, running_turn, user_message,
};
use crate::domain::services::{ChatService, StreamService};
use crate::infra::db::entity::{chat, chat_turn, message, message_attachment};
use crate::infra::db::repos::{
    AttachmentRepo, ChatRepo, MessageAttachmentRepo, MessageRepo, ThreadSummaryRepo, TurnRepo,
};
use crate::infra::db::tx::with_retry;
use crate::infra::metrics::MiniChatMetrics;

/// Dependencies of [`TurnService`].
pub struct TurnDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub chats: Arc<ChatService>,
    pub streams: Arc<StreamService>,
    pub outbox: Arc<dyn OutboxPort>,
    pub metrics: Arc<MiniChatMetrics>,
}

/// A validated mutation target: the authorized scope, the chat and its
/// latest, terminal turn requested by the caller.
#[derive(Debug, Clone)]
pub(crate) struct MutationTarget {
    pub scope: AccessScope,
    pub chat: chat::Model,
    pub turn: chat_turn::Model,
}

/// Turn reads and mutations.
pub struct TurnService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    chats: Arc<ChatService>,
    streams: Arc<StreamService>,
    outbox: Arc<dyn OutboxPort>,
    metrics: Arc<MiniChatMetrics>,
}

/// `result` label of a mutation outcome.
fn mutation_result<T>(r: &Result<T, DomainError>) -> &'static str {
    if r.is_ok() { "ok" } else { "error" }
}

impl TurnService {
    #[must_use]
    pub fn new(d: TurnDeps) -> Self {
        Self {
            db: d.db,
            clock: d.clock,
            chats: d.chats,
            streams: d.streams,
            outbox: d.outbox,
            metrics: d.metrics,
        }
    }

    /// Authoritative state of the turn `request_id` of the chat.
    ///
    /// # Errors
    /// `ChatNotFound`; `TurnNotFound` for a missing or soft-deleted turn;
    /// authorization and database failures.
    pub async fn status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        let (scope, chat) = self
            .chats
            .load_scoped(ctx, authz::READ_TURN, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let turn = TurnRepo
            .find_by_request(&conn, &scope, chat.id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::TurnNotFound)?;
        let state = TurnStatusState::from_internal(&turn.state)
            .ok_or_else(|| DomainError::Internal(format!("unknown turn state {}", turn.state)))?;
        Ok(TurnStatusView {
            request_id: turn.request_id,
            state,
            error_code: turn.error_code,
            assistant_message_id: turn.assistant_message_id,
            updated_at: turn.updated_at,
        })
    }

    /// Retry the latest turn: re-submit its user message (content and
    /// non-deleted attachments) as a new turn with a server-generated
    /// `request_id`.
    ///
    /// # Errors
    /// Preview: `ChatNotFound`, `TurnNotFound`, `AuthzDenied`,
    /// `TurnNotTerminal`, `NotLatestTurn`, `AuthzUnavailable`. Preflight
    /// (nothing changed): `InvalidModel`, `TooManyImages`,
    /// `FeatureDisabled`, `QuotaExceeded`, `VisionNotSupported`,
    /// `InputTooLong`. Commit: `NotLatestTurn`, `GenerationInProgress`.
    /// After the commit (the new turn is marked failed):
    /// `ContextBudgetExceeded`, `QuotaExceeded`, provider resolution.
    pub async fn retry(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<LiveTurn, DomainError> {
        let started = Instant::now();
        let r = self.retry_turn(ctx, chat_id, request_id).await;
        self.metrics
            .turn_mutation("retry", mutation_result(&r), started.elapsed());
        r
    }

    async fn retry_turn(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<LiveTurn, DomainError> {
        let target = self
            .preview(&ctx, chat_id, request_id, authz::RETRY_TURN)
            .await?;
        let (content, attachment_ids) = self.original_message(&target).await?;
        self.regenerate(
            ctx,
            target,
            content,
            attachment_ids,
            TurnMutationAuditEventType::TurnRetry,
        )
        .await
    }

    /// Edit the latest turn: a new turn with `content` and the original
    /// message's non-deleted attachments.
    ///
    /// # Errors
    /// `EmptyContent` (checked first), then as [`TurnService::retry`].
    pub async fn edit(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<LiveTurn, DomainError> {
        let started = Instant::now();
        let r = self.edit_turn(ctx, chat_id, request_id, content).await;
        self.metrics
            .turn_mutation("edit", mutation_result(&r), started.elapsed());
        r
    }

    async fn edit_turn(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<LiveTurn, DomainError> {
        if content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let target = self
            .preview(&ctx, chat_id, request_id, authz::EDIT_TURN)
            .await?;
        let (_, attachment_ids) = self.original_message(&target).await?;
        self.regenerate(
            ctx,
            target,
            content,
            attachment_ids,
            TurnMutationAuditEventType::TurnEdit,
        )
        .await
    }

    /// Soft-delete the latest turn and its messages (no new turn).
    ///
    /// # Errors
    /// As the preview and commit of [`TurnService::retry`]; outbox and
    /// database failures.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let started = Instant::now();
        let r = self.delete_turn(ctx, chat_id, request_id).await;
        self.metrics
            .turn_mutation("delete", mutation_result(&r), started.elapsed());
        r
    }

    async fn delete_turn(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let target = self
            .preview(ctx, chat_id, request_id, authz::DELETE_TURN)
            .await?;
        let now = self.clock.now();
        let event = mutation_event(
            ctx,
            &target.chat,
            TurnMutationAuditEventType::TurnDelete,
            target.turn.request_id,
            None,
            now,
        );
        let outbox = Arc::clone(&self.outbox);
        let MutationTarget { scope, turn, .. } = target;
        let wakes = with_retry(&self.db, move |tx| {
            let (scope, turn, outbox, event) = (
                scope.clone(),
                turn.clone(),
                Arc::clone(&outbox),
                event.clone(),
            );
            Box::pin(async move {
                retire(tx, &scope, &turn, None, now).await?;
                let mut wakes = PendingWakes::new();
                outbox.enqueue_audit(tx, &event, &mut wakes).await?;
                Ok(wakes)
            })
        })
        .await?;
        wakes.fire_all();
        Ok(())
    }

    /// Read-only validation of a mutation (one PDP call for `action`), in
    /// the order: turn exists (404) → requested by the caller (403) →
    /// terminal (400 `turn_state`) → latest non-deleted turn (409
    /// `NOT_LATEST_TURN`).
    pub(crate) async fn preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        action: &str,
    ) -> Result<MutationTarget, DomainError> {
        let (scope, chat) = self.chats.load_scoped(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let turn = TurnRepo
            .find_by_request(&conn, &scope, chat.id, request_id)
            .await?
            .ok_or(DomainError::TurnNotFound)?;
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AuthzDenied);
        }
        if turn.state == "running" {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = TurnRepo.find_latest_active(&conn, &scope, chat.id).await?;
        if latest.map(|t| t.id) != Some(turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        Ok(MutationTarget { scope, chat, turn })
    }

    /// Content and non-deleted attachment ids of the target's user message.
    async fn original_message(
        &self,
        target: &MutationTarget,
    ) -> Result<(String, Vec<Uuid>), DomainError> {
        let conn = self.db.conn()?;
        let (scope, chat_id) = (&target.scope, target.chat.id);
        let msg = MessageRepo
            .find_turn_message(&conn, scope, chat_id, target.turn.request_id, "user")
            .await?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "turn {} has no user message",
                    target.turn.request_id
                ))
            })?;
        let linked: Vec<Uuid> = MessageAttachmentRepo
            .list_for_message(&conn, scope, chat_id, msg.id)
            .await?
            .into_iter()
            .map(|l| l.attachment_id)
            .collect();
        let live = AttachmentRepo
            .find_in_chat(&conn, scope, chat_id, &linked)
            .await?;
        let ids = linked
            .into_iter()
            .filter(|id| live.iter().any(|a| a.id == *id && a.deleted_at.is_none()))
            .collect();
        Ok((msg.content, ids))
    }

    /// Retry / edit after the preview: preflight, commit, setup + stream.
    async fn regenerate(
        &self,
        ctx: SecurityContext,
        target: MutationTarget,
        content: String,
        attachment_ids: Vec<Uuid>,
        kind: TurnMutationAuditEventType,
    ) -> Result<LiveTurn, DomainError> {
        let input = TurnInput {
            content,
            attachment_ids,
            web_search_enabled: target.turn.web_search_enabled,
        };
        let pre = self
            .streams
            .mutation_preflight(
                &ctx,
                &target.scope,
                &target.chat,
                &input,
                target.turn.request_id,
            )
            .await?;
        let new = self.commit(&ctx, &target, &input, kind).await?;
        let MutationTarget { scope, chat, .. } = target;
        self.streams
            .start_committed_turn(ctx, scope, chat, pre, &input, new)
            .await
    }

    /// Mutation commit of a retry / edit (one transaction, reusing the
    /// preview's scope): re-check latest + terminal, soft-delete the old
    /// turn (`replaced_by_request_id`) and its messages, drop a covering
    /// thread summary, insert the new user message with the copied
    /// attachment links and the new `running` turn (preflight columns
    /// NULL), bump `chats.updated_at`, enqueue the audit event.
    ///
    /// # Errors
    /// `NotLatestTurn`; `GenerationInProgress` when the new turn loses the
    /// insert race on the one-running-turn-per-chat index; outbox and
    /// database failures.
    pub(crate) async fn commit(
        &self,
        ctx: &SecurityContext,
        target: &MutationTarget,
        input: &TurnInput,
        kind: TurnMutationAuditEventType,
    ) -> Result<NewTurn, DomainError> {
        let now = self.clock.now();
        let new = NewTurn {
            turn_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            started_at: now,
        };
        let mut msg = user_message(&target.chat, new.request_id, &input.content, now);
        msg.id = new.user_message_id;
        let turn_row = running_turn(
            &target.chat,
            ctx.subject_id(),
            new.turn_id,
            new.request_id,
            None,
            input.web_search_enabled,
            now,
        );
        let links: Vec<message_attachment::Model> = input
            .attachment_ids
            .iter()
            .map(|id| message_attachment::Model {
                tenant_id: msg.tenant_id,
                chat_id: msg.chat_id,
                message_id: msg.id,
                attachment_id: *id,
                created_at: now,
            })
            .collect();
        let event = mutation_event(
            ctx,
            &target.chat,
            kind,
            target.turn.request_id,
            Some(new.request_id),
            now,
        );
        let outbox = Arc::clone(&self.outbox);
        let (scope, old) = (target.scope.clone(), target.turn.clone());
        let result = with_retry(&self.db, move |tx| {
            let (scope, old, msg, links, turn_row, event, outbox) = (
                scope.clone(),
                old.clone(),
                msg.clone(),
                links.clone(),
                turn_row.clone(),
                event.clone(),
                Arc::clone(&outbox),
            );
            Box::pin(async move {
                retire(tx, &scope, &old, Some(new.request_id), now).await?;
                MessageRepo.insert(tx, &scope, msg).await?;
                for link in links {
                    MessageAttachmentRepo.insert(tx, &scope, link).await?;
                }
                TurnRepo.insert(tx, &scope, turn_row).await?;
                ChatRepo.touch(tx, &scope, old.chat_id, now).await?;
                let mut wakes = PendingWakes::new();
                outbox.enqueue_audit(tx, &event, &mut wakes).await?;
                Ok(wakes)
            })
        })
        .await;
        match result {
            Ok(wakes) => {
                wakes.fire_all();
                Ok(new)
            }
            // Fresh ids everywhere: only the running-turn index can clash.
            Err(e) if e.is_unique_violation() => Err(DomainError::GenerationInProgress),
            Err(e) => Err(e),
        }
    }
}

/// Inside the mutation transaction: soft-delete the old turn (guarded:
/// not deleted, terminal; on `PostgreSQL` this takes the row lock), check
/// that no newer non-deleted turn exists, soft-delete the turn's messages
/// and apply the summary rule (D "Summary Interaction on Turn Mutation").
async fn retire(
    runner: &impl DBRunner,
    scope: &AccessScope,
    old: &chat_turn::Model,
    replaced_by: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let rows = TurnRepo
        .soft_delete_terminal(runner, scope, old.id, replaced_by, now)
        .await?;
    if rows == 0 {
        return Err(DomainError::NotLatestTurn);
    }
    if let Some(newer) = TurnRepo
        .find_latest_active(runner, scope, old.chat_id)
        .await?
        && (newer.started_at, newer.id) > (old.started_at, old.id)
    {
        return Err(DomainError::NotLatestTurn);
    }
    let user_msg = MessageRepo
        .find_turn_message(runner, scope, old.chat_id, old.request_id, "user")
        .await?;
    MessageRepo
        .soft_delete_turn(runner, scope, old.chat_id, old.request_id, now)
        .await?;
    if let Some(msg) = user_msg {
        drop_covering_summary(runner, scope, &msg).await?;
    }
    Ok(())
}

/// Delete the chat's thread summary when its frontier is at or after the
/// mutated turn's user message, and clear `is_compressed` on the chat.
async fn drop_covering_summary(
    runner: &impl DBRunner,
    scope: &AccessScope,
    user_msg: &message::Model,
) -> Result<(), DomainError> {
    let Some(summary) = ThreadSummaryRepo
        .find_by_chat(runner, scope, user_msg.chat_id)
        .await?
    else {
        return Ok(());
    };
    let frontier = (
        summary.summarized_up_to_created_at,
        summary.summarized_up_to_message_id,
    );
    if frontier >= (user_msg.created_at, user_msg.id) {
        ThreadSummaryRepo
            .delete(runner, scope, user_msg.chat_id, summary.id)
            .await?;
        MessageRepo
            .clear_compressed(runner, scope, user_msg.chat_id)
            .await?;
    }
    Ok(())
}

/// `turn_retry` / `turn_edit` (`original_request_id` + `new_request_id`)
/// or `turn_delete` (`request_id`).
fn mutation_event(
    ctx: &SecurityContext,
    chat: &chat::Model,
    kind: TurnMutationAuditEventType,
    target_request_id: Uuid,
    new_request_id: Option<Uuid>,
    now: OffsetDateTime,
) -> MiniChatAuditEvent {
    let replaced = new_request_id.is_some();
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: kind,
        actor_user_id: ctx.subject_id(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        original_request_id: replaced.then_some(target_request_id),
        new_request_id,
        request_id: (!replaced).then_some(target_request_id),
        timestamp: now,
    })
}
