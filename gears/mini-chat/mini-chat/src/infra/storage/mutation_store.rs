//! The persistence of the turn mutations and of the reactions over the database provider
//! (feature `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`): the latest-turn query, the
//! mutation transaction, the `Existing` reserve, the unstarted-turn CAS and the reaction upsert
//! and removal.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, QueryFilter, Set};
use time::{Duration, OffsetDateTime};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DbTx, ScopeError, SecureEntityExt, SecureInsertExt, TxConfig};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError};
use crate::domain::messages::{MessageRow, Reaction};
use crate::domain::mutations::service::CommittedReplacement;
use crate::domain::mutations::store::Replacement;
use crate::domain::mutations::{
    CommitCommand, CommittedMutation, ExistingReserve, MutationReads, MutationStore, MutationTurn,
    ReactionStore, StoredReaction, UnstartedFailure, build_mutation_audit, check_latest,
    frontier_covers,
};
use crate::domain::quota::{QuotaService, ReserveReceipt, ReserveTurn, TurnIdentity};
use crate::domain::streaming::TurnOutbox;
use crate::domain::turns::{TurnRow, TurnState};
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::entity::message_reaction;
use crate::infra::storage::message_repo::MessageRepo;
use crate::infra::storage::repo_impl::{ChatRecord, ChatRepo};
use crate::infra::storage::stream_store::{tenant_chat, time_storage};
use crate::infra::storage::turn_repo::{AttachmentRepo, TurnRepo, UnreservedTurn};

/// The sanitized `error_detail` of an unstarted replacement turn.
const SETUP_FAILURE_DETAIL: &str = "the replacement turn could not be set up";

/// `time` truncated to microseconds, the precision every supported database keeps.
fn micros(time: OffsetDateTime) -> OffsetDateTime {
    time.replace_microsecond(time.microsecond()).unwrap_or(time)
}

/// The error of the mutation transaction: a domain rejection, or a driver failure kept as it is
/// so that the platform retry helper can tell transient lock contention (`SQLite` `BUSY`, a
/// `PostgreSQL` deadlock) from every other failure.
#[derive(Debug)]
enum MutationTxError {
    Domain(DomainError),
    Db(DbError),
}

impl From<DomainError> for MutationTxError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl From<DbError> for MutationTxError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

impl From<ScopeError> for MutationTxError {
    fn from(error: ScopeError) -> Self {
        match error {
            ScopeError::Db(db) => Self::Db(DbError::Sea(db)),
            other => Self::Domain(DomainError::from(other)),
        }
    }
}

impl From<MutationTxError> for DomainError {
    fn from(error: MutationTxError) -> Self {
        match error {
            MutationTxError::Domain(error) => error,
            MutationTxError::Db(error) => Self::from(error),
        }
    }
}

/// The driver error of a failed attempt, for the contention check of the retry helper.
fn driver_error(error: &MutationTxError) -> Option<&DbErr> {
    match error {
        MutationTxError::Db(DbError::Sea(db)) => Some(db),
        MutationTxError::Db(_) | MutationTxError::Domain(_) => None,
    }
}

fn not_latest() -> DomainError {
    DomainError::Conflict {
        reason: ConflictReason::NotLatestTurn,
    }
}

#[async_trait]
impl MutationStore for DBProvider<DomainError> {
    async fn find_mutation_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTurn>, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-select-target
        let Some(model) = TurnRepo::find_model(&conn, &chat, request_id).await? else {
            return Ok(None);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-select-target
        let web_search_enabled = model.web_search_enabled;
        Ok(Some(MutationTurn {
            row: crate::infra::storage::turn_repo::turn_row(model)?,
            web_search_enabled,
        }))
    }

    async fn latest_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        TurnRepo::latest(&conn, &chat, time_storage(self)).await
    }

    async fn target_user_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        MessageRepo::user_message_of(&conn, &chat, request_id).await
    }

    async fn read_mutation(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        target_request_id: Uuid,
    ) -> Result<MutationReads, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        let storage = time_storage(self);
        let boundary = MessageRepo::latest_message(
            &conn,
            chat.tenant_id,
            chat.id,
            storage,
            Some(target_request_id),
        )
        .await?;
        let linked = match MessageRepo::user_message_of(&conn, &chat, target_request_id).await? {
            Some(message) => MessageRepo::linked_attachments(&conn, &chat, message.id).await?,
            None => Vec::new(),
        };
        let (ready_documents, ready_code_interpreter) =
            AttachmentRepo::chat_facts(&conn, &chat).await?;
        Ok(MutationReads {
            boundary,
            linked,
            ready_documents,
            ready_code_interpreter,
        })
    }

    async fn commit_mutation(
        &self,
        outbox: Arc<dyn TurnOutbox>,
        command: &CommitCommand,
    ) -> Result<CommittedMutation, DomainError> {
        let owner = AccessScope::for_tenant(command.tenant_id).ensure_owner(command.user_id);
        let chat = {
            let conn = self.conn()?;
            ChatRepo::load_chat(&conn, &owner, command.chat_id).await?
        };
        let storage = time_storage(self);
        // transient lock contention restarts the whole transaction a bounded number of times
        // (the platform helper); a lost race is a 409 and is never retried
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-begin
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-first-commit-wins
        let outcome = self
            .db()
            .transaction_with_retry(TxConfig::default(), driver_error, |tx| {
                let outbox = Arc::clone(&outbox);
                let chat = chat.clone();
                let owner = owner.clone();
                let command = command.clone();
                Box::pin(
                    async move { mutate_in_tx(tx, outbox, &chat, &owner, &command, storage).await },
                )
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-first-commit-wins
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-begin
        match outcome {
            Ok((committed, wake)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-commit
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-return
                // the outbox wake fires only after the commit
                wake.fire();
                Ok(committed)
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-return
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-commit
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-loser-clean
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-no-server-retry
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-db-error
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-500
            Err(error) => Err(resolve_commit_failure(self, &chat, command, error.into()).await),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-500
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-db-error
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-no-server-retry
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-loser-clean
        }
    }

    async fn reserve_existing(
        &self,
        quota: &QuotaService,
        command: &ExistingReserve,
    ) -> Result<ReserveReceipt, DomainError> {
        let tx_quota = quota.clone();
        let identity = TurnIdentity {
            tenant_id: command.tenant_id,
            user_id: command.user_id,
            chat_id: command.chat_id,
            request_id: command.request_id,
        };
        let decision = command.decision.clone();
        let turn_id = command.turn_id;
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-reserved
        self.transaction(move |tx| {
            Box::pin(async move {
                tx_quota
                    .write_reserve(tx, &identity, &decision, &ReserveTurn::Existing { turn_id })
                    .await
            })
        })
        .await
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-reserved
    }

    async fn fail_unstarted(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        code: UnstartedFailure,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-no-settlement
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-replacement-turn
        TurnRepo::fail_running(
            &conn,
            tenant_id,
            turn_id,
            code.as_str(),
            SETUP_FAILURE_DETAIL,
            micros(OffsetDateTime::now_utc()),
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-replacement-turn
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-no-settlement
    }
}

/// The classification of a failed mutation transaction. A unique violation of the replacement
/// insert is told apart after the rollback: a turn or user message with the new `request_id` is a
/// server UUID collision (500), otherwise the running-turn index was lost (409).
async fn resolve_commit_failure(
    db: &DBProvider<DomainError>,
    chat: &ChatRecord,
    command: &CommitCommand,
    error: DomainError,
) -> DomainError {
    let DomainError::UniqueViolation { .. } = error else {
        return error;
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-running-race
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-key-collision
    let Some(replacement) = command.replacement.as_ref() else {
        return DomainError::internal("a delete violated a unique constraint");
    };
    let taken = async {
        let conn = db.conn()?;
        let turn = TurnRepo::find_by_key(&conn, chat, replacement.request_id).await?;
        if turn.is_some() {
            return Ok::<_, DomainError>(true);
        }
        let message = MessageRepo::user_message_of(&conn, chat, replacement.request_id).await?;
        Ok(message.is_some())
    }
    .await;
    match taken {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-key-collision
        Ok(true) => DomainError::internal("the new request_id collides with an existing row"),
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-key-collision
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-generation-in-progress
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-running-index
        Ok(false) => DomainError::Conflict {
            reason: ConflictReason::GenerationInProgress,
        },
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-running-index
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-generation-in-progress
        Err(error) => error,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-key-collision
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-running-race
}

/// The body of the mutation transaction, in the order of the contract: the lock and re-check,
/// the guarded soft-delete of the turn and of its messages, the replacement message, links and
/// turn, the activity bump, the summary invalidation and the audit enqueue.
async fn mutate_in_tx(
    tx: &DbTx<'_>,
    outbox: Arc<dyn TurnOutbox>,
    chat: &ChatRecord,
    owner: &AccessScope,
    command: &CommitCommand,
    storage: TimeStorage,
) -> Result<(CommittedMutation, Wake), MutationTxError> {
    let now = micros(command.now);
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-recheck
    let user_message = lock_and_recheck(tx, chat, command, storage).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-recheck

    // the guarded soft-delete of the turn and of its messages
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-lost
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-soft-delete-lost
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-to-replaced
    // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-to-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-old-request-id
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-no-refund
    let replaced_by = command.replacement.as_ref().map(|r| r.request_id);
    if !TurnRepo::soft_delete(tx, chat, command.target_request_id, replaced_by, now).await? {
        return Err(not_latest().into());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-no-refund
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-old-request-id
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-to-deleted
    // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility:p1:inst-tv-to-replaced
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-soft-delete-lost
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-lost
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-messages
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-count
    MessageRepo::soft_delete_turn_messages(tx, chat, command.target_request_id, now).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-count
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-soft-delete-messages

    let mut committed = CommittedMutation {
        op: command.op,
        target_request_id: command.target_request_id,
        replacement: None,
        summary_invalidated: false,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-replace
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-activity
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-delete-no-activity
    if let Some(replacement) = &command.replacement {
        committed.replacement =
            Some(insert_replacement(tx, chat, command.user_id, replacement, now, storage).await?);
        // the message-activity contract of retry and edit; a delete leaves the chat untouched
        ChatRepo::touch_chat(tx, owner, chat.id).await?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-delete-no-activity
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-activity
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-replace

    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-after-soft-delete
    committed.summary_invalidated = invalidate_summary(tx, chat, &user_message).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-after-soft-delete
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-summary

    // the audit event, in the same transaction
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-audit
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-payload-too-large
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-exactly-one
    let event = build_mutation_audit(
        command.op,
        command.user_id,
        chat.id,
        command.target_request_id,
        replaced_by,
        now,
    )?;
    let wake = outbox
        .enqueue_mutation_audit(tx, chat.tenant_id, &event)
        .await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-exactly-one
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-payload-too-large
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-audit
    // the soft-deletes, the replacement message and links, the replacement turn, the summary
    // deletion, the activity bump and the audit enqueue commit together or not at all
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-atomic
    Ok((committed, wake))
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-atomic
}

/// The lock and re-check of the target turn (DESIGN 3.9 rule 7): the transaction takes the write
/// lock with its first statement (the database write lock on `SQLite`, the row lock of the
/// target on `PostgreSQL`), then reads the target, the latest turn and the user message again.
async fn lock_and_recheck(
    tx: &DbTx<'_>,
    chat: &ChatRecord,
    command: &CommitCommand,
    storage: TimeStorage,
) -> Result<MessageRow, MutationTxError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-lock
    if TurnRepo::lock_for_mutation(tx, chat, command.target_request_id).await? == 0 {
        return Err(not_latest().into());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-lock
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-reread
    let target = TurnRepo::find_by_key(tx, chat, command.target_request_id)
        .await?
        .ok_or_else(not_latest)?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-reread
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-match
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-deleted
    if target.deleted_at.is_some() {
        return Err(not_latest().into());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-deleted
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-match
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-deleted
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-not-terminal
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-not-terminal
    if target.state == TurnState::Running {
        return Err(DomainError::TurnNotTerminal.into());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-not-terminal
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-not-terminal
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-not-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-not-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-newer
    let latest = TurnRepo::latest(tx, chat, storage).await?;
    check_latest(&target, latest.as_ref())?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-case-newer
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return-not-latest
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-not-latest
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-latest
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return
    Ok(
        MessageRepo::user_message_of(tx, chat, command.target_request_id)
            .await?
            .ok_or_else(|| DomainError::internal("the target turn has no user message"))?,
    )
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck:p1:inst-lr-return
}

/// The replacement user message (timestamp after every message of the chat), its carried links
/// and the replacement `running` turn.
async fn insert_replacement(
    tx: &DbTx<'_>,
    chat: &ChatRecord,
    user_id: Uuid,
    replacement: &Replacement,
    now: OffsetDateTime,
    storage: TimeStorage,
) -> Result<CommittedReplacement, DomainError> {
    // the later of the application clock and the latest message of the chat, soft-deleted ones
    // included, plus one microsecond
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-user-after-all
    let created_at = match MessageRepo::latest_message_any(tx, chat, storage).await? {
        Some(latest) if latest.created_at >= now => latest.created_at + Duration::microseconds(1),
        _ => now,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-user-after-all
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-insert-user-message
    MessageRepo::insert_user_message(
        tx,
        chat,
        replacement.user_message_id,
        replacement.request_id,
        &replacement.content,
        created_at,
    )
    .await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-insert-user-message
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-copy-links
    if !replacement.attachment_ids.is_empty() {
        MessageRepo::link_attachments(
            tx,
            chat,
            replacement.user_message_id,
            &replacement.attachment_ids,
            created_at,
        )
        .await?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-copy-links
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-insert-turn
    TurnRepo::insert_unreserved(
        tx,
        chat,
        UnreservedTurn {
            id: replacement.turn_id,
            request_id: replacement.request_id,
            user_id,
            web_search_enabled: replacement.web_search_enabled,
            started_at: replacement.started_at,
            now,
        },
    )
    .await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-insert-turn
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-return
    Ok(CommittedReplacement {
        turn_id: replacement.turn_id,
        request_id: replacement.request_id,
        user_message_id: replacement.user_message_id,
        user_message_created_at: created_at,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-return
}

/// The summary invalidation, after the soft-deletes: the chat's summary is deleted (and
/// `is_compressed` cleared) when its frontier is at or after the target user message.
async fn invalidate_summary(
    tx: &DbTx<'_>,
    chat: &ChatRecord,
    user_message: &MessageRow,
) -> Result<bool, MutationTxError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-select
    // the row lock orders a concurrent summary commit and this mutation (PostgreSQL); without a
    // row the lock matches nothing
    MessageRepo::lock_summary(tx, chat).await?;
    let summary = MessageRepo::thread_summary(tx, chat).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-select
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-no-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-none
    let Some(summary) = summary else {
        return Ok(false);
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-none
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-no-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-keep
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-kept
    // a summary that stops before the target user message stays: the next context is the
    // summary followed by the replacement turn
    if !frontier_covers(
        (summary.frontier_created_at, summary.frontier_message_id),
        (user_message.created_at, user_message.id),
    ) {
        return Ok(false);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-kept
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-keep
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-no-generation
    // only deletions: a summary is never generated, enqueued or rewritten here
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-delete
    MessageRepo::delete_summary(tx, chat).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-delete
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-clear-compressed
    MessageRepo::clear_compressed(tx, chat).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-clear-compressed
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-no-generation
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-invalidated
    Ok(true)
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-return-invalidated
}

#[async_trait]
impl ReactionStore for DBProvider<DomainError> {
    async fn upsert_reaction(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: Reaction,
        now: OffsetDateTime,
    ) -> Result<StoredReaction, DomainError> {
        let conn = self.conn()?;
        let created_at = micros(now);
        // the tenant and the user come from the security context, never from input
        let scope = AccessScope::for_tenant(tenant_id).ensure_owner(user_id);
        let row = message_reaction::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(message_id),
            user_id: Set(user_id),
            tenant_id: Set(tenant_id),
            reaction: Set(reaction.as_str().to_owned()),
            created_at: Set(created_at),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-upsert
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-one-row
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-idempotent
        // one statement: insert, or replace `reaction` and `created_at` of the existing row
        message_reaction::Entity::insert(row.clone())
            .secure()
            .scope_with_model(&scope, &row)?
            .on_conflict_raw(
                OnConflict::columns([
                    message_reaction::Column::MessageId,
                    message_reaction::Column::UserId,
                ])
                .update_columns([
                    message_reaction::Column::Reaction,
                    message_reaction::Column::CreatedAt,
                ])
                .to_owned(),
            )
            .exec(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-idempotent
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-one-row
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-upsert
        // read back the stored row (an upsert keeps the id of the existing one); a database error
        // is the generic 500 through the error conversion
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-return-500
        let stored = message_reaction::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(message_id))
                    .add(message_reaction::Column::UserId.eq(user_id)),
            )
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("the stored reaction is missing"))?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-return-500
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-return
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-no-side-effects
        Ok(StoredReaction {
            message_id,
            reaction,
            created_at: stored.created_at,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-no-side-effects
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert:p1:inst-ru-return
    }

    async fn remove_reaction(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        use toolkit_db::secure::SecureDeleteExt;
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-idempotent
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-owner-only
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-return-500
        message_reaction::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::MessageId.eq(message_id))
                    .add(message_reaction::Column::UserId.eq(user_id)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id).ensure_owner(user_id))
            .exec(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-return-500
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-db-error
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-owner-only
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-idempotent
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-no-side-effects
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-return
        // no audit event, no outbox message and no chat activity bump
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-return
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal:p1:inst-rr-no-side-effects
    }
}

#[cfg(test)]
#[path = "mutation_repo_tests.rs"]
mod mutation_repo_tests;

#[cfg(test)]
#[path = "reaction_repo_tests.rs"]
mod reaction_repo_tests;

#[cfg(test)]
#[path = "mutation_concurrency_tests.rs"]
mod mutation_concurrency_tests;
