//! Turn status (DESIGN "Turn Status API") and the mutations of the latest turn: retry, edit and
//! delete (DESIGN 3.9 "Turn Mutation Rules").
//!
//! A mutation is previewed read-only first (latest terminal turn of the caller). Delete then
//! commits; retry and edit hand over to [`StreamService::run_mutation`], which runs the preflight
//! of the re-submitted message, this module's [`commit_mutation`] and the stream setup.

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use opentelemetry::KeyValue;
use sea_orm::ActiveValue::{NotSet, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Authz, ChatAction};
use crate::domain::error::DomainError;
use crate::domain::stream::setup::user_message_row;
use crate::domain::stream::{EventStream, MutationPlan, Replacement, StreamService};
use crate::infra::db::entity::{chat_turns, chats};
use crate::infra::db::repo;
use crate::infra::db::repo::messages::Position;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::db::{MessageRole, TurnState};
use crate::infra::outbox::{OutboxEnqueuer, OutboxRecord};
use crate::metrics::Metrics;

/// Turn state as reported by the status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Running,
    Done,
    Error,
    Cancelled,
}

impl From<TurnState> for TurnStatus {
    fn from(state: TurnState) -> Self {
        match state {
            TurnState::Running => Self::Running,
            TurnState::Completed => Self::Done,
            TurnState::Failed => Self::Error,
            TurnState::Cancelled => Self::Cancelled,
        }
    }
}

/// The status of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: TurnStatus,
    /// Terminal error code of a failed turn.
    pub error_code: Option<String>,
    /// The persisted assistant message, when there is one.
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

/// A turn mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    Retry,
    Edit,
    Delete,
}

impl MutationOp {
    /// The `op` metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }

    const fn audit_event_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }

    const fn action(self) -> ChatAction {
        match self {
            Self::Retry => ChatAction::RetryTurn,
            Self::Edit => ChatAction::EditTurn,
            Self::Delete => ChatAction::DeleteTurn,
        }
    }
}

/// The replacement turn a retry/edit inserts in the mutation commit.
#[derive(Debug, Clone)]
pub struct NewTurn {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub user_message_id: Uuid,
    /// The user message (original content for a retry).
    pub content: String,
    /// The non-deleted attachments of the original user message (linked to the new one).
    pub attachment_ids: Vec<Uuid>,
    /// Copied from the replaced turn.
    pub web_search_enabled: bool,
}

/// Input of [`commit_mutation`].
#[derive(Debug, Clone)]
pub struct MutationCommit {
    pub op: MutationOp,
    /// Tenant scope of the authorized chat.
    pub scope: AccessScope,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// The requesting user.
    pub actor: Uuid,
    /// The previewed target turn.
    pub target: chat_turns::Model,
    /// The replacement (retry/edit); `None` for a delete.
    pub new_turn: Option<NewTurn>,
}

/// The mutation transaction (DESIGN 3.9 rules 1–7 and "Summary Interaction on Turn Mutation"):
/// 1. soft-delete the target when it is still terminal and not deleted (the first write: the
///    row lock / `SQLite` write lock serializes concurrent mutations), then re-check that no
///    newer live turn exists;
/// 2. soft-delete its messages;
/// 3. retry/edit: insert the new user message, copy the live attachment links and insert the new
///    `running` turn without preflight columns (losing the running-turn race is
///    `GenerationInProgress`);
/// 4. delete the thread summary (and clear `is_compressed`) when its frontier is at or after the
///    target's user message;
/// 5. bump `chats.updated_at` and enqueue the mutation audit event.
///
/// # Errors
/// `NotLatestTurn`, `TurnNotTerminal`, `GenerationInProgress`, `OutboxPayloadTooLarge`,
/// `Internal`.
pub async fn commit_mutation(
    db: &DBProvider<DomainError>,
    outbox: &Arc<OutboxEnqueuer>,
    commit: MutationCommit,
) -> Result<(), DomainError> {
    let outbox = Arc::clone(outbox);
    write_tx_with_wakes(db, move |tx, wakes| {
        let (c, outbox) = (commit.clone(), Arc::clone(&outbox));
        Box::pin(async move {
            let now = db_now();
            replace_target(tx, &c, now).await?;
            let user_message = repo::messages::find_of_turn(
                tx,
                &c.scope,
                c.chat_id,
                c.target.request_id,
                MessageRole::User,
            )
            .await?;
            repo::messages::soft_delete_of_turn(tx, &c.scope, c.chat_id, c.target.request_id, now)
                .await?;
            if let Some(new_turn) = &c.new_turn {
                insert_new_turn(tx, &c, new_turn, now).await?;
            }
            if let Some(m) = user_message {
                drop_covering_summary(tx, &c.scope, c.chat_id, Position::of(&m)).await?;
            }
            repo::chats::touch_updated_at(tx, c.chat_id, now).await?;
            let record = OutboxRecord::audit(&audit_event(&c, now))?;
            wakes.add(outbox.enqueue(tx, record).await?);
            Ok(())
        })
    })
    .await
    .map_err(|err| match err {
        DomainError::Conflict {
            code: "unique_violation",
        } => DomainError::GenerationInProgress,
        other => other,
    })
}

/// Step 1 of [`commit_mutation`].
async fn replace_target(
    tx: &DbTx<'_>,
    c: &MutationCommit,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let replaced_by = c.new_turn.as_ref().map(|t| t.request_id);
    if !repo::turns::soft_delete_terminal(tx, &c.scope, c.target.id, replaced_by, now).await? {
        // Changed since the preview: running again cannot happen, so it was deleted.
        let current =
            repo::turns::find_by_request(tx, &c.scope, c.chat_id, c.target.request_id).await?;
        return Err(match current {
            Some(t) if t.state == TurnState::Running.as_str() && t.deleted_at.is_none() => {
                DomainError::TurnNotTerminal
            }
            _ => DomainError::NotLatestTurn,
        });
    }
    if let Some(latest) = repo::turns::latest(tx, &c.scope, c.chat_id).await?
        && (latest.started_at, latest.id) > (c.target.started_at, c.target.id)
    {
        return Err(DomainError::NotLatestTurn);
    }
    Ok(())
}

/// Step 3 of [`commit_mutation`].
async fn insert_new_turn(
    tx: &DbTx<'_>,
    c: &MutationCommit,
    t: &NewTurn,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let message = user_message_row(
        c.tenant_id,
        c.chat_id,
        t.user_message_id,
        t.request_id,
        t.content.clone(),
        now,
    );
    repo::messages::insert(tx, &c.scope, message).await?;
    repo::attachments::link_to_message(
        tx,
        &c.scope,
        c.tenant_id,
        c.chat_id,
        t.user_message_id,
        &t.attachment_ids,
        now,
    )
    .await?;
    repo::turns::insert(tx, &c.scope, unstarted_turn(c, t, now)).await
}

/// The new `running` turn of a retry/edit: the preflight columns stay NULL until its reserve.
fn unstarted_turn(c: &MutationCommit, t: &NewTurn, now: OffsetDateTime) -> chat_turns::ActiveModel {
    chat_turns::ActiveModel {
        id: Set(t.turn_id),
        tenant_id: Set(c.tenant_id),
        chat_id: Set(c.chat_id),
        request_id: Set(t.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(c.actor)),
        state: Set(TurnState::Running.as_str().to_owned()),
        provider_name: NotSet,
        provider_response_id: NotSet,
        assistant_message_id: NotSet,
        error_code: NotSet,
        reserve_tokens: Set(None),
        max_output_tokens_applied: Set(None),
        reserved_credits_micro: Set(None),
        policy_version_applied: Set(None),
        effective_model: Set(None),
        minimal_generation_floor_applied: Set(None),
        error_detail: NotSet,
        deleted_at: NotSet,
        replaced_by_request_id: NotSet,
        started_at: Set(now),
        last_progress_at: Set(Some(now)),
        web_search_enabled: Set(t.web_search_enabled),
        web_search_completed_count: NotSet,
        code_interpreter_completed_count: NotSet,
        file_search_completed_count: NotSet,
        completed_at: NotSet,
        updated_at: Set(now),
    }
}

/// Step 4 of [`commit_mutation`]: a summary whose inclusive frontier is at or after `mutated`
/// (the target's user message) covers the mutated turn.
async fn drop_covering_summary(
    tx: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    mutated: Position,
) -> Result<(), DomainError> {
    let Some(summary) = repo::thread_summaries::find_for_chat(tx, scope, chat_id).await? else {
        return Ok(());
    };
    let frontier = (
        summary.summarized_up_to_created_at,
        summary.summarized_up_to_message_id,
    );
    if frontier >= (mutated.created_at, mutated.id) {
        repo::thread_summaries::delete_for_chat(tx, scope, chat_id).await?;
        repo::messages::clear_compressed(tx, scope, chat_id).await?;
    }
    Ok(())
}

fn audit_event(c: &MutationCommit, now: OffsetDateTime) -> AuditEvent {
    let (original_request_id, new_request_id, request_id) = match &c.new_turn {
        Some(t) => (Some(c.target.request_id), Some(t.request_id), None),
        None => (None, None, Some(c.target.request_id)),
    };
    AuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: c.op.audit_event_type().to_owned(),
        tenant_id: c.tenant_id,
        actor_user_id: c.actor,
        chat_id: c.chat_id,
        original_request_id,
        new_request_id,
        request_id,
        timestamp: now,
    })
}

/// Reads and mutates the turns of a chat.
pub struct TurnService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
    stream: Arc<StreamService>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
}

impl TurnService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<Authz>,
        stream: Arc<StreamService>,
        outbox: Arc<OutboxEnqueuer>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            db,
            authz,
            stream,
            outbox,
            metrics,
        }
    }

    /// Retries the latest turn `request_id`: re-submits its user message (and attachments) as a
    /// new turn and returns that turn's live stream. Run it in a spawned task (see
    /// [`StreamService::send`]).
    ///
    /// # Errors
    /// The preview errors (see [`Self::preview`]) and those of
    /// [`StreamService::run_mutation`].
    pub async fn retry(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<EventStream, DomainError> {
        self.replace(ctx, chat_id, request_id, Replacement::Retry)
            .await
    }

    /// Edits the latest turn `request_id`: submits `content` (with the original attachments) as
    /// a new turn and returns that turn's live stream. Run it in a spawned task.
    ///
    /// # Errors
    /// `EmptyContent` when `content` is blank, then like [`Self::retry`].
    pub async fn edit(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<EventStream, DomainError> {
        self.replace(ctx, chat_id, request_id, Replacement::Edit(content))
            .await
    }

    /// Soft-deletes the latest turn `request_id` and its messages.
    ///
    /// # Errors
    /// The preview errors (see [`Self::preview`]) and those of [`commit_mutation`].
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let op = MutationOp::Delete;
        let started = Instant::now();
        let result = async {
            let (chat, scope, target) = self.preview(ctx, chat_id, request_id, op).await?;
            commit_mutation(
                &self.db,
                &self.outbox,
                MutationCommit {
                    op,
                    scope,
                    tenant_id: chat.tenant_id,
                    chat_id,
                    actor: ctx.subject_id(),
                    target,
                    new_turn: None,
                },
            )
            .await
        }
        .await;
        self.record(op, started, result.is_ok());
        result
    }

    async fn replace(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        replacement: Replacement,
    ) -> Result<EventStream, DomainError> {
        let op = match replacement {
            Replacement::Retry => MutationOp::Retry,
            Replacement::Edit(_) => MutationOp::Edit,
        };
        let started = Instant::now();
        let result = async {
            if let Replacement::Edit(content) = &replacement
                && content.trim().is_empty()
            {
                return Err(DomainError::EmptyContent);
            }
            let (chat, scope, target) = self.preview(ctx, chat_id, request_id, op).await?;
            let plan = MutationPlan {
                replacement,
                scope,
                target,
            };
            self.stream.run_mutation(ctx, chat, plan).await
        }
        .await;
        self.record(op, started, result.is_ok());
        result
    }

    /// The read-only mutation preview: authorization, the chat, then the target turn of
    /// `(chat_id, request_id)` must exist, be terminal, be the latest non-deleted turn and
    /// belong to the caller. Returns the chat, its tenant scope and the target.
    ///
    /// # Errors
    /// `AccessDenied` / `AuthzUnavailable` (PDP), `ChatNotFound`, `TurnNotFound`,
    /// `TurnNotTerminal`, `NotLatestTurn`, `AccessDenied` (another requester), `Internal`.
    async fn preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        op: MutationOp,
    ) -> Result<(chats::Model, AccessScope, chat_turns::Model), DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, op.action(), Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = repo::chats::load_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound {
                id: chat_id.to_string(),
            })?;
        let scope = scope.tenant_only();
        let target = repo::turns::find_by_request(&conn, &scope, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::TurnNotFound {
                id: request_id.to_string(),
            })?;
        if target.state == TurnState::Running.as_str() {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = repo::turns::latest(&conn, &scope, chat_id).await?;
        if target.deleted_at.is_some() || latest.is_none_or(|l| l.id != target.id) {
            return Err(DomainError::NotLatestTurn);
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AccessDenied);
        }
        Ok((chat, scope, target))
    }

    /// `turn_mutation{op,result}` and `turn_mutation_latency_ms{op}`, after the outcome is known
    /// (every transaction has committed or rolled back).
    #[allow(clippy::cast_precision_loss)] // histogram sample in milliseconds
    fn record(&self, op: MutationOp, started: Instant, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.metrics.turn_mutation.add(
            1,
            &[
                KeyValue::new("op", op.as_str()),
                KeyValue::new("result", result),
            ],
        );
        self.metrics.turn_mutation_latency_ms.record(
            started.elapsed().as_millis() as f64,
            &[KeyValue::new("op", op.as_str())],
        );
    }

    /// The status of the turn of `(chat_id, request_id)` of one of the caller's chats.
    ///
    /// # Errors
    /// `ChatNotFound`, `TurnNotFound` (unknown or soft-deleted), `AccessDenied` /
    /// `AuthzUnavailable`, `Internal` for an unknown stored state or a database failure.
    pub async fn status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusView, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::ReadTurn, Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        repo::chats::load_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound {
                id: chat_id.to_string(),
            })?;
        let turn = repo::turns::find_by_request(&conn, &scope.tenant_only(), chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or_else(|| DomainError::TurnNotFound {
                id: request_id.to_string(),
            })?;
        let state = TurnState::parse(&turn.state).ok_or_else(|| {
            DomainError::Internal(format!("stored turn state `{}` is not valid", turn.state))
        })?;
        Ok(TurnStatusView {
            request_id: turn.request_id,
            state: state.into(),
            error_code: turn.error_code,
            assistant_message_id: turn.assistant_message_id,
            updated_at: turn.updated_at,
        })
    }
}

#[cfg(test)]
mod tests;
