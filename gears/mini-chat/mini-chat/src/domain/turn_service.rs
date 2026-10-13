//! Turn mutations: retry, edit and delete of the latest turn (DESIGN section
//! 3.9 "Turn Mutation Rules (P1)", section 3.6 "Retry / edit variant",
//! section 3.8 "Retry and edit evaluate the PDP once").
//!
//! **Mutation preview** ([`TurnService::preview`], read-only), in this
//! order:
//!
//! 1. edit only: non-blank `content` (`EMPTY_CONTENT`), before anything else;
//! 2. PEP `retry_turn` / `edit_turn` / `delete_turn` on the chat (evaluated
//!    once; the mutation reuses the returned scope), scoped chat load (404
//!    chat);
//! 3. the turn of `(chat_id, request_id)`: unknown is 404 (turn), a
//!    soft-deleted turn is `409 NOT_LATEST_TURN`;
//! 4. a `running` target is `400 failed_precondition` (`turn_state` /
//!    `STATE`), checked before the latest-turn rule;
//! 5. the target must be the latest live turn (greatest `(started_at, id)`,
//!    a newer running turn included), else `409 NOT_LATEST_TURN`;
//! 6. the target's `requester_user_id` must be the caller, else `403
//!    permission_denied` (`AUTHZ_DENIED`).
//!
//! **Retry / edit**: preview, then the stream service's mutation preflight
//! (a rejection changes nothing), then the **mutation commit** — one
//! transaction whose first statement is a write (the guarded soft delete of
//! the target, which also takes the row lock on PostgreSQL):
//!
//! - soft delete of the target (`deleted_at`, `replaced_by_request_id` = the
//!   server-generated UUID v4) — guarded by `deleted_at IS NULL` and a
//!   terminal state; when it matches nothing the target is re-read and the
//!   preview error is reported (`NOT_LATEST_TURN` / `STATE`);
//! - re-check that no live turn follows the target (`SELECT ... FOR UPDATE`
//!   on PostgreSQL), else `NOT_LATEST_TURN`;
//! - soft delete of the target's messages;
//! - the `chats.updated_at` bump, guarded by `deleted_at IS NULL`: a chat
//!   soft-deleted since the preview is `404` (chat) and the transaction
//!   rolls back. The chat row is locked before the running-turn insert, the
//!   same order as the `messages:stream` reserve transaction, so the two
//!   cannot deadlock on PostgreSQL;
//! - the replacement user message (retry: the original content; edit: the
//!   new content) and the copy of the links to non-deleted attachments;
//! - the new `running` turn with NULL preflight columns, the original
//!   `web_search_enabled` and `started_at` = the preflight instant (the
//!   reserve periods are computed from it, the orphan watchdog derives them
//!   from `started_at`); losing the running-turn index is `409
//!   GENERATION_IN_PROGRESS`;
//! - the summary interaction (below) and the `turn_retry` / `turn_edit`
//!   audit event (enqueued last).
//!
//! The stream service then assembles the context, resolves the provider and
//! takes the reserve, and the new turn streams like `messages:stream`.
//!
//! **Delete**: preview, then one transaction (no preflight, no streaming):
//! the guarded soft delete (no replacement), the newer-turn re-check, the
//! soft delete of the messages, the summary interaction and the
//! `turn_delete` audit event.
//!
//! **Summary interaction**: when the chat's summary frontier
//! `(summarized_up_to_created_at, summarized_up_to_message_id)` is at or
//! after the target's user message `(created_at, id)`, the summary row is
//! deleted and `is_compressed` is cleared on every message of the chat, in
//! the mutation transaction; otherwise the summary is kept.

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use toolkit_db::Db;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::DbTx;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{Action, AuthzService};
use crate::domain::chat_service::chat_not_found;
use crate::domain::error::{
    AbortReason, DomainError, InvalidArgumentReason, PreconditionSubject, ResourceKind,
};
use crate::domain::ports::{
    AttachmentLinkCopy, ChatRecord, MessageKey, MessageRecord, MutationTargetRecord,
    NewMutationTurn, NewUserMessage, Repositories, TurnSoftDelete, TurnState, storage_now,
};
use crate::domain::stream::service::{MutationPreflightInput, MutationTurn};
use crate::domain::stream::{SendOptions, StreamService, StreamStart};

/// A mutation of the latest turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnMutation {
    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    Retry,
    /// `PATCH /v1/chats/{id}/turns/{request_id}` with the new content.
    Edit { content: String },
    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    Delete,
}

impl TurnMutation {
    /// The PEP action of the mutation.
    #[must_use]
    pub const fn action(&self) -> Action {
        match self {
            Self::Retry => Action::RetryTurn,
            Self::Edit { .. } => Action::EditTurn,
            Self::Delete => Action::DeleteTurn,
        }
    }
}

/// The read-only result of a successful mutation preview.
#[derive(Debug, Clone)]
pub struct MutationPreview {
    /// The scope of the single PDP evaluation of the mutation.
    pub scope: AccessScope,
    pub chat: ChatRecord,
    /// The latest live, terminal turn of the caller.
    pub target: MutationTargetRecord,
    /// The target's live user message (`None` only for inconsistent data).
    pub user_message: Option<MessageRecord>,
}

impl MutationPreview {
    /// The order key the summary frontier is compared with: the user
    /// message, or the turn start when the message is missing.
    fn user_key(&self) -> MessageKey {
        self.user_message.as_ref().map_or(
            MessageKey {
                created_at: self.target.started_at,
                id: Uuid::nil(),
            },
            |message| MessageKey {
                created_at: message.created_at,
                id: message.id,
            },
        )
    }
}

/// The replacement turn of a retry / edit to commit.
struct Replacement {
    kind: TurnMutation,
    request_id: Uuid,
    turn_id: Uuid,
    user_message_id: Uuid,
    content: String,
    /// The preflight instant of the replacement turn: its `started_at`, so
    /// the reserve periods (computed from it) and the orphan settlement
    /// periods (derived from `started_at`) are the same rows.
    started_at: time::OffsetDateTime,
}

/// Turn mutation domain service.
pub struct TurnService {
    db: Db,
    repos: Repositories,
    authz: Arc<AuthzService>,
    stream: Arc<StreamService>,
}

impl std::fmt::Debug for TurnService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnService").finish_non_exhaustive()
    }
}

impl TurnService {
    /// Builds the service over the gear database (mutation transactions),
    /// the persistence ports, the PEP and the stream service (preflight and
    /// streaming of the replacement turn).
    #[must_use]
    pub fn new(
        db: Db,
        repos: Repositories,
        authz: Arc<AuthzService>,
        stream: Arc<StreamService>,
    ) -> Self {
        Self {
            db,
            repos,
            authz,
            stream,
        }
    }

    /// The read-only mutation preview (module docs).
    ///
    /// # Errors
    ///
    /// `EMPTY_CONTENT` (edit), PEP errors, 404 (chat, turn), `400
    /// turn_state`, `409 NOT_LATEST_TURN`, `403 AUTHZ_DENIED` (requester);
    /// [`DomainError::Internal`] on storage failure.
    pub async fn preview(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        mutation: &TurnMutation,
    ) -> Result<MutationPreview, DomainError> {
        if let TurnMutation::Edit { content } = mutation
            && content.trim().is_empty()
        {
            return Err(DomainError::invalid_argument(
                InvalidArgumentReason::EmptyContent,
            ));
        }
        let scope = self
            .authz
            .authorize_chat(ctx, mutation.action(), Some(chat_id))
            .await?;
        let chat = self
            .repos
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let target = self
            .repos
            .turns
            .find_mutation_target(&scope, chat_id, request_id)
            .await?
            .ok_or_else(|| turn_not_found(request_id))?;
        if target.deleted {
            return Err(DomainError::aborted(AbortReason::NotLatestTurn));
        }
        if target.state == TurnState::Running {
            return Err(DomainError::failed_precondition(
                PreconditionSubject::TurnState,
            ));
        }
        let latest = self.repos.turns.latest_active(&scope, chat_id).await?;
        if latest != Some(target.key()) {
            return Err(DomainError::aborted(AbortReason::NotLatestTurn));
        }
        if target.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::PermissionDenied {
                resource: ResourceKind::Turn,
            });
        }
        let user_message = self
            .repos
            .messages
            .find_user_of_request(&scope, chat_id, request_id)
            .await?;
        Ok(MutationPreview {
            scope,
            chat,
            target,
            user_message,
        })
    }

    /// Retries the latest turn: the original content is re-submitted as a
    /// new turn with a server-generated `request_id`.
    ///
    /// # Errors
    ///
    /// The preview errors, the mutation preflight errors (429 / 400, nothing
    /// changed), `409 NOT_LATEST_TURN` / `GENERATION_IN_PROGRESS` from the
    /// commit, the setup errors after the commit (the new turn is then
    /// `failed`); [`DomainError::Internal`] on storage failure.
    pub async fn retry(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        options: SendOptions,
    ) -> Result<StreamStart, DomainError> {
        self.replace(ctx, chat_id, request_id, TurnMutation::Retry, options)
            .await
    }

    /// Edits the latest turn: `content` replaces the user message of a new
    /// turn with a server-generated `request_id`.
    ///
    /// # Errors
    ///
    /// `EMPTY_CONTENT`, then the errors of [`Self::retry`].
    pub async fn edit(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
        options: SendOptions,
    ) -> Result<StreamStart, DomainError> {
        self.replace(
            ctx,
            chat_id,
            request_id,
            TurnMutation::Edit { content },
            options,
        )
        .await
    }

    /// Deletes the latest turn (soft delete, no replacement, no preflight).
    ///
    /// # Errors
    ///
    /// The preview errors, `409 NOT_LATEST_TURN` from the commit;
    /// [`DomainError::Internal`] on storage or outbox failure.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let preview = self
            .preview(ctx, chat_id, request_id, &TurnMutation::Delete)
            .await?;
        self.commit_delete(ctx, preview).await?;
        tracing::info!(
            target: "mini_chat.turns",
            chat_id = %chat_id,
            request_id = %request_id,
            "turn deleted"
        );
        Ok(())
    }

    /// The delete transaction of a previewed target (module docs).
    async fn commit_delete(
        &self,
        ctx: &SecurityContext,
        preview: MutationPreview,
    ) -> Result<(), DomainError> {
        let repos = self.repos.clone();
        let actor = ctx.subject_id();
        in_transaction::<_, (), DomainError>(&self.db, move |tx| {
            Box::pin(async move {
                let now = storage_now();
                let unit = MutationUnit {
                    tx,
                    repos: &repos,
                    preview: &preview,
                };
                unit.remove_target(None, now).await?;
                unit.summary_interaction().await?;
                let wake = unit
                    .enqueue_audit(TurnMutationAuditEvent::delete(
                        preview.chat.tenant_id,
                        actor,
                        preview.chat.id,
                        preview.target.request_id,
                        now,
                    ))
                    .await?;
                Ok(((), wake))
            })
        })
        .await
    }

    async fn replace(
        &self,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        mutation: TurnMutation,
        options: SendOptions,
    ) -> Result<StreamStart, DomainError> {
        let preview = self.preview(&ctx, chat_id, request_id, &mutation).await?;
        let content = match &mutation {
            TurnMutation::Edit { content } => content.clone(),
            TurnMutation::Retry | TurnMutation::Delete => preview
                .user_message
                .as_ref()
                .map(|message| message.content.clone())
                .ok_or_else(|| {
                    DomainError::internal(format!(
                        "turn {} has no live user message to retry",
                        preview.target.id
                    ))
                })?,
        };
        let preflight = self
            .stream
            .mutation_preflight(
                &ctx,
                MutationPreflightInput {
                    scope: &preview.scope,
                    chat: &preview.chat,
                    content: &content,
                    web_search_enabled: preview.target.web_search_enabled,
                    replaced_request_id: preview.target.request_id,
                    source_message_id: preview.user_message.as_ref().map(|m| m.id),
                },
            )
            .await?;
        let replacement = Replacement {
            kind: mutation,
            request_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            content,
            started_at: preflight.decision.at,
        };
        let turn = self.commit(&ctx, preview, replacement).await?;
        tracing::info!(
            target: "mini_chat.turns",
            chat_id = %chat_id,
            original_request_id = %request_id,
            new_request_id = %turn.request_id,
            "turn replaced"
        );
        self.stream
            .start_mutation(ctx, turn, preflight, options)
            .await
    }

    /// The retry / edit mutation commit (module docs).
    async fn commit(
        &self,
        ctx: &SecurityContext,
        preview: MutationPreview,
        replacement: Replacement,
    ) -> Result<MutationTurn, DomainError> {
        let repos = self.repos.clone();
        let actor = ctx.subject_id();
        in_transaction::<_, MutationTurn, DomainError>(&self.db, move |tx| {
            Box::pin(async move {
                let now = storage_now();
                let unit = MutationUnit {
                    tx,
                    repos: &repos,
                    preview: &preview,
                };
                unit.remove_target(Some(replacement.request_id), now)
                    .await?;
                let chat = &preview.chat;
                // The chat row is locked before the running-turn insert (the
                // order of the `messages:stream` reserve transaction); a chat
                // soft-deleted since the preview rolls everything back.
                if !repos
                    .chats
                    .touch_in_tx(tx, &preview.scope, chat.id, now)
                    .await?
                {
                    return Err(chat_not_found(chat.id));
                }
                repos
                    .messages
                    .insert_user_in_tx(
                        tx,
                        &preview.scope,
                        NewUserMessage {
                            id: replacement.user_message_id,
                            tenant_id: chat.tenant_id,
                            chat_id: chat.id,
                            request_id: replacement.request_id,
                            content: replacement.content.clone(),
                            created_at: now,
                        },
                    )
                    .await?;
                if let Some(source) = &preview.user_message {
                    repos
                        .attachments
                        .copy_live_links_in_tx(
                            tx,
                            &preview.scope,
                            AttachmentLinkCopy {
                                tenant_id: chat.tenant_id,
                                chat_id: chat.id,
                                from_message_id: source.id,
                                to_message_id: replacement.user_message_id,
                                at: now,
                            },
                        )
                        .await?;
                }
                repos
                    .turns
                    .insert_mutation_turn_in_tx(
                        tx,
                        &preview.scope,
                        NewMutationTurn {
                            id: replacement.turn_id,
                            tenant_id: chat.tenant_id,
                            chat_id: chat.id,
                            request_id: replacement.request_id,
                            requester_user_id: actor,
                            web_search_enabled: preview.target.web_search_enabled,
                            now: replacement.started_at,
                        },
                    )
                    .await
                    .map_err(|err| match err {
                        DomainError::Aborted {
                            reason: AbortReason::TurnAlreadyRunning,
                        } => DomainError::aborted(AbortReason::GenerationInProgress),
                        other => other,
                    })?;
                unit.summary_interaction().await?;
                let build = if matches!(replacement.kind, TurnMutation::Edit { .. }) {
                    TurnMutationAuditEvent::edit
                } else {
                    TurnMutationAuditEvent::retry
                };
                let event = build(
                    chat.tenant_id,
                    actor,
                    chat.id,
                    preview.target.request_id,
                    replacement.request_id,
                    now,
                );
                let wake = unit.enqueue_audit(event).await?;
                Ok((
                    MutationTurn {
                        scope: preview.scope.clone(),
                        chat: preview.chat.clone(),
                        turn_id: replacement.turn_id,
                        request_id: replacement.request_id,
                        user_message_id: replacement.user_message_id,
                        content: replacement.content,
                    },
                    wake,
                ))
            })
        })
        .await
    }
}

/// The shared steps of one mutation transaction.
struct MutationUnit<'t, 'a> {
    tx: &'t DbTx<'a>,
    repos: &'t Repositories,
    preview: &'t MutationPreview,
}

impl MutationUnit<'_, '_> {
    /// The guarded soft delete of the target (the first statement, a write),
    /// the newer-turn re-check and the soft delete of the target's messages.
    async fn remove_target(
        &self,
        replaced_by_request_id: Option<Uuid>,
        now: time::OffsetDateTime,
    ) -> Result<(), DomainError> {
        let preview = self.preview;
        let target = &preview.target;
        let turns = &self.repos.turns;
        let removed = turns
            .soft_delete_terminal_in_tx(
                self.tx,
                &preview.scope,
                TurnSoftDelete {
                    turn_id: target.id,
                    replaced_by_request_id,
                    at: now,
                },
            )
            .await?;
        if !removed {
            return Err(self.not_mutable().await);
        }
        if turns
            .newer_active_exists_in_tx(self.tx, &preview.scope, preview.chat.id, target.key())
            .await?
        {
            return Err(DomainError::aborted(AbortReason::NotLatestTurn));
        }
        self.repos
            .messages
            .soft_delete_request_in_tx(
                self.tx,
                &preview.scope,
                preview.chat.id,
                target.request_id,
                now,
            )
            .await?;
        Ok(())
    }

    /// The preview error of a target the guarded soft delete did not match.
    async fn not_mutable(&self) -> DomainError {
        let preview = self.preview;
        match self
            .repos
            .turns
            .find_mutation_target_in_tx(
                self.tx,
                &preview.scope,
                preview.chat.id,
                preview.target.request_id,
            )
            .await
        {
            Ok(Some(turn)) if !turn.deleted && turn.state == TurnState::Running => {
                DomainError::failed_precondition(PreconditionSubject::TurnState)
            }
            Ok(Some(_)) => DomainError::aborted(AbortReason::NotLatestTurn),
            Ok(None) => turn_not_found(preview.target.request_id),
            Err(err) => err,
        }
    }

    /// "Summary Interaction on Turn Mutation" (module docs).
    async fn summary_interaction(&self) -> Result<(), DomainError> {
        let preview = self.preview;
        let messages = &self.repos.messages;
        let Some(frontier) = messages
            .summary_frontier_in_tx(self.tx, &preview.scope, preview.chat.id)
            .await?
        else {
            return Ok(());
        };
        if covers(frontier, preview.user_key()) {
            messages
                .drop_thread_summary_in_tx(self.tx, &preview.scope, preview.chat.id)
                .await?;
        }
        Ok(())
    }

    /// Enqueues the mutation audit event (the last statement).
    async fn enqueue_audit(&self, event: TurnMutationAuditEvent) -> Result<Wake, DomainError> {
        self.repos
            .outbox
            .enqueue_audit_in_tx(self.tx, &AuditEvent::from(event))
            .await
    }
}

/// Whether a summary with `frontier` covers the message `key` (frontier at or
/// after it in the `(created_at, id)` order).
#[must_use]
pub fn covers(frontier: MessageKey, key: MessageKey) -> bool {
    (frontier.created_at, frontier.id) >= (key.created_at, key.id)
}

fn turn_not_found(request_id: Uuid) -> DomainError {
    DomainError::not_found(ResourceKind::Turn, request_id.to_string())
}

#[cfg(test)]
#[path = "turn_service_tests.rs"]
mod tests;
