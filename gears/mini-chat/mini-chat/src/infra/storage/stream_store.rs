//! The message and send-setup persistence ports over the database provider (slice
//! S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`): [`MessageStore`] for the
//! messages list and the turn status, and [`StreamStore`] for the send setup including the
//! reserve transaction.

use async_trait::async_trait;
use time::{Duration, OffsetDateTime};
use toolkit_db::DBProvider;
use toolkit_db::secure::DbTx;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError, ResourceKind};
use crate::domain::messages::{MessageRow, MessageStore, MessageView, project_messages};
use crate::domain::quota::{QuotaService, ReserveTurn, TurnIdentity};
use crate::domain::streaming::{
    ContextReads, PreflightReads, ReserveCommand, ReservedTurn, SnapshotBoundary, StreamStore,
};
use crate::domain::turns::TurnRow;
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::message_repo::MessageRepo;
use crate::infra::storage::repo_impl::{ChatRecord, ChatRepo};
use crate::infra::storage::turn_repo::{AttachmentRepo, NewTurn, TurnRepo};

/// How the timestamps are stored on the engine of the provider (`SQLite` keeps text).
pub(crate) fn time_storage(db: &DBProvider<DomainError>) -> TimeStorage {
    TimeStorage::of_engine(db.db().db_engine())
}

/// The chat of a tenant whose owner-scoped load already happened at admission.
pub(crate) async fn tenant_chat(
    db: &DBProvider<DomainError>,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<ChatRecord, DomainError> {
    let conn = db.conn()?;
    ChatRepo::load_chat(&conn, &AccessScope::for_tenant(tenant_id), chat_id).await
}

/// `time` truncated to microseconds, the precision every supported database keeps.
fn micros(time: OffsetDateTime) -> OffsetDateTime {
    time.replace_microsecond(time.microsecond()).unwrap_or(time)
}

#[async_trait]
impl MessageStore for DBProvider<DomainError> {
    async fn list_messages(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let conn = self.conn()?;
        // the owner-scoped chat load: a missing, soft-deleted, foreign-user or foreign-tenant
        // chat is the same not-found of the chat type
        let chat = ChatRepo::load_chat(&conn, scope, chat_id).await?;
        let page = MessageRepo::list_page(&conn, &chat, query, time_storage(self)).await?;
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-project
        // one batch for the attachments and one for the caller's reactions of the whole page
        let message_ids: Vec<Uuid> = page.items.iter().map(|row| row.id).collect();
        let attachments = MessageRepo::attachment_batch(&conn, &chat, &message_ids).await?;
        let reactions = MessageRepo::reaction_batch(&conn, &chat, user_id, &message_ids).await?;
        let items = project_messages(page.items, &attachments, &reactions)?;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-project
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn load_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnRow, DomainError> {
        let conn = self.conn()?;
        let chat = ChatRepo::load_chat(&conn, scope, chat_id).await?;
        TurnRepo::find_active(&conn, &chat, request_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: ResourceKind::Turn,
                id: Some(request_id.to_string()),
            })
    }
}

#[async_trait]
impl StreamStore for DBProvider<DomainError> {
    async fn find_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        TurnRepo::find_by_key(&conn, &chat, request_id).await
    }

    async fn has_running_turn(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        TurnRepo::has_running(&conn, &chat).await
    }

    async fn load_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        MessageRepo::find_message(&conn, &chat, message_id).await
    }

    async fn read_preflight(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<PreflightReads, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        let boundary = MessageRepo::snapshot_boundary(&conn, &chat, time_storage(self)).await?;
        let attachments = AttachmentRepo::referenced(&conn, &chat, attachment_ids).await?;
        let (ready_documents, ready_code_interpreter) =
            AttachmentRepo::chat_facts(&conn, &chat).await?;
        Ok(PreflightReads {
            boundary,
            attachments,
            ready_documents,
            ready_code_interpreter,
        })
    }

    async fn read_context(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: Option<&SnapshotBoundary>,
        limit: u32,
    ) -> Result<ContextReads, DomainError> {
        let chat = tenant_chat(self, tenant_id, chat_id).await?;
        let conn = self.conn()?;
        let summary = MessageRepo::thread_summary(&conn, &chat).await?;
        let recent = MessageRepo::recent_messages(
            &conn,
            &chat,
            boundary,
            summary.as_ref(),
            limit,
            time_storage(self),
        )
        .await?;
        Ok(ContextReads { summary, recent })
    }

    async fn reserve(
        &self,
        quota: &QuotaService,
        command: &ReserveCommand,
    ) -> Result<ReservedTurn, DomainError> {
        let owner = AccessScope::for_tenant(command.tenant_id).ensure_owner(command.user_id);
        let chat = {
            let conn = self.conn()?;
            ChatRepo::load_chat(&conn, &owner, command.chat_id).await?
        };
        let storage = time_storage(self);
        let tx_chat = chat.clone();
        let tx_command = command.clone();
        let tx_owner = owner.clone();
        // the transaction closure owns its collaborators (the quota handle shares them)
        let tx_quota = quota.clone();
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-begin
        // one transaction: committed as a whole or rolled back as a whole
        let outcome = self
            .transaction(move |tx| {
                Box::pin(async move {
                    reserve_in_tx(tx, &tx_quota, &tx_chat, &tx_owner, &tx_command, storage).await
                })
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-begin
        match outcome {
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-send-race
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-key-race
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-key-race
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-running-race
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-running-race
            // an insert violated the `(chat_id, request_id)` key, the `(chat_id, request_id,
            // role)` message index or the running-turn index; the transaction rolled back, so
            // the key is re-read: a turn with the `request_id` is a `request_id_conflict`,
            // otherwise another turn is running
            Err(DomainError::UniqueViolation { .. }) => {
                let conn = self.conn()?;
                let existing = TurnRepo::find_by_key(&conn, &chat, command.request_id).await?;
                let reason = if existing.is_some() {
                    ConflictReason::RequestIdConflict
                } else {
                    ConflictReason::TurnAlreadyRunning
                };
                Err(DomainError::Conflict { reason })
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-running-race
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-running-race
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-key-race
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-key-race
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-send-race
            other => other,
        }
    }
}

/// The body of the reserve transaction, in the order of the contract: the quota reserve with its
/// re-check, the user message, the activity bump, the attachment validation and links, and the
/// `running` turn.
async fn reserve_in_tx(
    tx: &DbTx<'_>,
    quota: &QuotaService,
    chat: &ChatRecord,
    owner: &AccessScope,
    command: &ReserveCommand,
    storage: TimeStorage,
) -> Result<ReservedTurn, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-reserve
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-over-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-429
    // the reserve write with `ReserveTurn::New` (the turn row is inserted below with the
    // preflight columns); an over-limit bucket is 429 `resource_exhausted` scope `tokens`
    // without any downgrade, and the error rolls the transaction back. A unique violation of
    // the quota rows is never a key race of this request.
    let identity = TurnIdentity {
        tenant_id: command.tenant_id,
        user_id: command.user_id,
        chat_id: command.chat_id,
        request_id: command.request_id,
    };
    let receipt = quota
        .write_reserve(tx, &identity, &command.decision, &ReserveTurn::New)
        .await
        .map_err(|error| match error {
            DomainError::UniqueViolation { .. } => {
                DomainError::internal("the quota reserve violated a unique constraint")
            }
            other => other,
        })?;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-429
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-over-limit
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-reserve

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-clock
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-across-turns
    // `created_at` is the application UTC clock reading, moved to one microsecond after the
    // latest message of the chat when the clock is not later; the parallel-turn guard makes the
    // new message follow the previous turn's messages
    let now = micros(command.now);
    let created_at = match MessageRepo::snapshot_boundary(tx, chat, storage).await? {
        Some(latest) if latest.created_at >= now => latest.created_at + Duration::microseconds(1),
        _ => now,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-across-turns
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-clock
    MessageRepo::insert_user_message(
        tx,
        chat,
        command.user_message_id,
        command.request_id,
        &command.content,
        created_at,
    )
    .await?;

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-activity
    // the message-activity contract: `chats.updated_at` moves in the same transaction
    ChatRepo::touch_chat(tx, owner, chat.id).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-activity

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-attachments
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-association-only
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-return
    // a rejection is 400 `invalid_attachment` and rolls the reserve and the message back; the
    // links are an association for UI, audit and history only (they neither scope nor filter
    // retrieval)
    AttachmentRepo::validate_for_send(tx, chat, command.user_id, &command.attachment_ids).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-association-only
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-attachments
    MessageRepo::link_attachments(
        tx,
        chat,
        command.user_message_id,
        &command.attachment_ids,
        created_at,
    )
    .await?;

    // the `running` turn with `started_at` equal to the preflight instant
    TurnRepo::insert_running(
        tx,
        chat,
        &NewTurn {
            id: command.turn_id,
            request_id: command.request_id,
            user_id: command.user_id,
            web_search_enabled: command.web_search_enabled,
            decision: &command.decision,
            now,
        },
    )
    .await?;

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-db-error
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-500
    // any other database error propagates and rolls everything back; the error mapping turns
    // it into a generic 500
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return-500
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-db-error
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return
    Ok(ReservedTurn {
        turn_id: command.turn_id,
        user_message_id: command.user_message_id,
        user_message_created_at: created_at,
        started_at: command.decision.started_at,
        receipt,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-return
}

#[cfg(test)]
#[path = "reserve_tx_tests.rs"]
mod reserve_tx_tests;
