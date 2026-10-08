//! Turn status (DESIGN section 3.3, Turn Status API) and the turn mutation
//! rules of DESIGN section 3.9 (preview, delete, and the mutation commit of
//! retry/edit).

use std::collections::HashSet;
use std::sync::Arc;

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::enums::TurnState;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, ChatAction};
use crate::domain::time::db_now;
use crate::infra::db::entities::{attachment, chat, chat_turn, message};
use crate::infra::db::repos::message_repo::NewMessage;
use crate::infra::db::repos::turn_repo::NewRunningTurn;
use crate::infra::db::repos::{attachment_repo, chat_repo, message_repo, turn_repo};
use crate::infra::outbox::MiniChatOutbox;

/// API turn state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

/// A tail-only turn mutation (DESIGN section 3.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationKind {
    Retry,
    Edit,
    Delete,
}

impl MutationKind {
    /// The PDP action of the mutation.
    #[must_use]
    pub const fn action(self) -> ChatAction {
        match self {
            Self::Retry => ChatAction::RetryTurn,
            Self::Edit => ChatAction::EditTurn,
            Self::Delete => ChatAction::DeleteTurn,
        }
    }

    /// `event_type` of the mutation audit event.
    #[must_use]
    pub const fn event_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

/// The result of a successful mutation preview: the chat, the target turn
/// (latest, live, terminal, owned by the caller), its live user message and
/// that message's live attachments (link order).
#[derive(Clone, Debug)]
pub struct MutationTarget {
    pub kind: MutationKind,
    pub actor_user_id: Uuid,
    pub chat: chat::Model,
    pub turn: chat_turn::Model,
    pub user_message: Option<message::Model>,
    pub attachments: Vec<attachment::Model>,
}

/// The rows a retry/edit mutation commit inserts: the new user message, its
/// copied attachment links and the new `running` turn (preflight columns
/// NULL, filled later in the reserve transaction).
#[derive(Clone, Debug)]
pub struct Replacement {
    pub message: NewMessage,
    pub attachment_ids: Vec<Uuid>,
    pub turn: NewRunningTurn,
}

pub struct TurnService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<dyn AuthzPort>,
    outbox: Arc<MiniChatOutbox>,
}

impl TurnService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<dyn AuthzPort>,
        outbox: Arc<MiniChatOutbox>,
    ) -> Self {
        Self { db, authz, outbox }
    }

    /// Read-only mutation preview (DESIGN section 3.9 and section 3.6
    /// retry/edit variant step 1): `chat_scope(action)` once, the chat (404),
    /// the target turn (missing: 404 turn; soft-deleted: `NotLatestTurn`),
    /// `running` (`TurnNotTerminal`, checked before the latest check),
    /// requester = caller (`NotRequester`), latest live turn by
    /// `(started_at, id)` (`NotLatestTurn`). Also reads the turn's live user
    /// message and its live attachments.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat or turn), `NotLatestTurn`,
    /// `TurnNotTerminal`, `NotRequester`, database failure.
    pub async fn preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        kind: MutationKind,
    ) -> Result<MutationTarget, DomainError> {
        let scope = self
            .authz
            .chat_scope(ctx, kind.action(), Some(chat_id))
            .await?;
        let conn = self.db.conn()?;
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let tenant_id = chat.tenant_id;
        let turn = turn_repo::find_by_request(&conn, tenant_id, chat.id, request_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Turn,
            })?;
        if turn.deleted_at.is_some() {
            return Err(DomainError::NotLatestTurn);
        }
        if turn.state == TurnState::Running.as_str() {
            return Err(DomainError::TurnNotTerminal);
        }
        let actor_user_id = ctx.subject_id();
        if turn.requester_user_id != Some(actor_user_id) {
            return Err(DomainError::NotRequester);
        }
        let latest = turn_repo::latest_live(&conn, tenant_id, chat.id).await?;
        if latest.map(|t| t.id) != Some(turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        let user_message =
            message_repo::find_turn_user_message(&conn, tenant_id, chat.id, request_id).await?;
        let attachments = match &user_message {
            Some(m) => attachment_repo::live_for_messages(&conn, tenant_id, chat.id, &[m.id])
                .await?
                .remove(&m.id)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(MutationTarget {
            kind,
            actor_user_id,
            chat,
            turn,
            user_message,
            attachments,
        })
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`: preview, then one
    /// transaction that soft-deletes the turn and its messages, applies the
    /// summary rule, bumps `chats.updated_at` and enqueues `turn_delete`.
    ///
    /// # Errors
    /// See [`Self::preview`]; `NotLatestTurn` when a concurrent mutation won,
    /// database or outbox failure.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let target = self
            .preview(ctx, chat_id, request_id, MutationKind::Delete)
            .await?;
        self.commit_delete(&target).await
    }

    /// The delete mutation commit for a previewed target.
    ///
    /// # Errors
    /// `NotLatestTurn` (lost race), database or outbox failure.
    pub async fn commit_delete(&self, target: &MutationTarget) -> Result<(), DomainError> {
        self.commit(target, None).await
    }

    /// The retry/edit mutation commit (DESIGN section 3.6 retry/edit variant
    /// step 3): soft-deletes the target turn (`replaced_by_request_id` = the
    /// new request id) and its messages, inserts the new user message, its
    /// attachment links and the new `running` turn, applies the summary
    /// rule, bumps `chats.updated_at` and enqueues `turn_retry` / `turn_edit`.
    ///
    /// # Errors
    /// `NotLatestTurn`, `GenerationInProgress` (lost the race for the one
    /// running turn of the chat), database or outbox failure. On error
    /// nothing is committed.
    pub async fn commit_replacement(
        &self,
        target: &MutationTarget,
        replacement: Replacement,
    ) -> Result<(), DomainError> {
        match self.commit(target, Some(replacement)).await {
            // The request id is fresh, so the only reachable unique index is
            // the one running turn per chat (DESIGN section 3.9, rule 7).
            Err(DomainError::UniqueViolation) => Err(DomainError::GenerationInProgress),
            other => other,
        }
    }

    async fn commit(
        &self,
        target: &MutationTarget,
        replacement: Option<Replacement>,
    ) -> Result<(), DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let target = target.clone();
        let wake = self
            .db
            .transaction(move |tx| {
                Box::pin(async move { apply_mutation(tx, &outbox, &target, replacement).await })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// Authoritative state of the turn `request_id` (`chat_turns`). A
    /// soft-deleted turn is `NotFound` (turn). `error_code` is reported only
    /// for `error`, `assistant_message_id` only for `done` and `cancelled`.
    ///
    /// # Errors
    /// Authorization failure, `NotFound` (chat or turn), `Internal` for an
    /// unknown stored state, database failure.
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
        let chat = chat_repo::find_scoped(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })?;
        let turn = turn_repo::find_by_request(&conn, chat.tenant_id, chat.id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Turn,
            })?;
        let state = TurnState::parse(&turn.state).ok_or_else(|| {
            DomainError::Internal(format!("turn {} has state {}", turn.id, turn.state))
        })?;
        let api_state = match state {
            TurnState::Running => TurnStatusState::Running,
            TurnState::Completed => TurnStatusState::Done,
            TurnState::Failed => TurnStatusState::Error,
            TurnState::Cancelled => TurnStatusState::Cancelled,
        };
        Ok(TurnStatusView {
            request_id: turn.request_id,
            state: api_state,
            error_code: turn
                .error_code
                .filter(|_| api_state == TurnStatusState::Error),
            assistant_message_id: turn.assistant_message_id.filter(|_| {
                matches!(
                    api_state,
                    TurnStatusState::Done | TurnStatusState::Cancelled
                )
            }),
            updated_at: turn.updated_at,
        })
    }
}

/// The mutation transaction body. Writes first (Ruling R5): the guarded
/// soft-delete of the target is also the terminal / not-deleted re-check, and
/// every read after it runs under the write lock.
async fn apply_mutation(
    tx: &DbTx<'_>,
    outbox: &MiniChatOutbox,
    target: &MutationTarget,
    replacement: Option<Replacement>,
) -> Result<Wake, DomainError> {
    let (tenant_id, chat_id) = (target.chat.tenant_id, target.chat.id);
    let old = &target.turn;
    let now = db_now();
    let new_request_id = replacement.as_ref().map(|r| r.turn.request_id);
    if turn_repo::soft_delete_terminal(tx, tenant_id, old.id, new_request_id, now).await? == 0 {
        return Err(lost_race(tx, target, replacement.is_some()).await?);
    }
    if let Some(latest) = turn_repo::latest_live(tx, tenant_id, chat_id).await?
        && (latest.started_at, latest.id) > (old.started_at, old.id)
    {
        return Err(DomainError::NotLatestTurn);
    }
    message_repo::soft_delete_turn(tx, tenant_id, chat_id, old.request_id, now).await?;
    if let Some(user) = &target.user_message
        && let Some(frontier) = message_repo::summary_frontier(tx, tenant_id, chat_id).await?
        && frontier >= (user.created_at, user.id)
    {
        message_repo::delete_thread_summary(tx, tenant_id, chat_id).await?;
        message_repo::clear_compressed(tx, tenant_id, chat_id).await?;
    }
    if let Some(r) = replacement {
        let message_id = r.message.id;
        message_repo::insert(tx, r.message).await?;
        // Only attachments still live at commit time are copied.
        let live: HashSet<Uuid> =
            attachment_repo::live_in_chat_by_ids(tx, tenant_id, chat_id, &r.attachment_ids)
                .await?
                .into_iter()
                .map(|a| a.id)
                .collect();
        let copied: Vec<Uuid> = r
            .attachment_ids
            .into_iter()
            .filter(|id| live.contains(id))
            .collect();
        attachment_repo::link_to_message(tx, tenant_id, chat_id, message_id, &copied, now).await?;
        turn_repo::insert_running(tx, r.turn).await?;
    }
    chat_repo::touch_updated_at(tx, tenant_id, chat_id, now).await?;
    let (original_request_id, request_id) = match new_request_id {
        Some(_) => (Some(old.request_id), None),
        None => (None, Some(old.request_id)),
    };
    let event = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: target.kind.event_type().to_owned(),
        tenant_id,
        chat_id,
        actor_user_id: target.actor_user_id,
        original_request_id,
        new_request_id,
        request_id,
        timestamp: now,
    });
    outbox.enqueue_audit(tx, tenant_id, &event).await
}

/// The error of a mutation whose guarded soft-delete matched no row: another
/// writer changed the target after the preview. A retry/edit that finds a
/// running turn lost the race for the chat's one running turn
/// (`GenerationInProgress`, DESIGN section 3.9 rule 7); otherwise the target
/// is no longer the latest live turn.
async fn lost_race(
    tx: &DbTx<'_>,
    target: &MutationTarget,
    replacing: bool,
) -> Result<DomainError, DomainError> {
    let (tenant_id, chat_id) = (target.chat.tenant_id, target.chat.id);
    let current = turn_repo::find_by_id(tx, tenant_id, target.turn.id).await?;
    Ok(match current {
        None => DomainError::NotFound {
            resource: ResourceKind::Turn,
        },
        Some(t) if t.deleted_at.is_none() => DomainError::TurnNotTerminal,
        Some(_)
            if replacing
                && turn_repo::find_running(tx, tenant_id, chat_id)
                    .await?
                    .is_some() =>
        {
            DomainError::GenerationInProgress
        }
        Some(_) => DomainError::NotLatestTurn,
    })
}

#[cfg(test)]
#[path = "turn_service_tests.rs"]
mod turn_service_tests;
