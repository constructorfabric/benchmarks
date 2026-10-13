//! Turns of a chat: the read-only status API (DESIGN §3.3 "Turn Status API") and
//! the tail-only mutations retry, edit and delete (DESIGN §3.9).
//!
//! Retry and edit follow the "Retry / edit variant" of DESIGN §3.6: a read-only
//! mutation preview (authorization, target checks), the model resolution and the
//! stream service's preflight run first, so a rejection leaves the previous answer
//! in place. Only then the mutation commits (one transaction), and the stream
//! service finishes the setup of the new turn ([`TurnMode::Existing`]) and
//! streams it like a send.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use sea_orm::DbBackend;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use super::chat::ChatService;
use super::model_catalog::ModelCatalogService;
use super::stream::{
    LiveStream, PreflightRequest, StreamService, TurnMode, TurnPrep, user_message,
};
use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{TurnState, error_codes};
use crate::infra::db::entities::{chat, chat_turn, message};
use crate::infra::db::repos::message::MessagePosition;
use crate::infra::db::repos::turn::{NewTurn, TerminalUpdate, TurnCounters};
use crate::infra::db::repos::{AttachmentRepo, ChatRepo, MessageRepo, ThreadSummaryRepo, TurnRepo};
use crate::infra::db::tx::with_tx_retry;
use crate::infra::outbox::payloads::AUDIT_PAYLOAD_TYPE;
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};

/// Infrastructure of [`TurnService`].
pub struct TurnDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<ChatAuthz>,
    pub chats: Arc<ChatService>,
    pub models: Arc<ModelCatalogService>,
    pub stream: Arc<StreamService>,
    pub outbox: Arc<OutboxEnqueuer>,
}

pub struct TurnService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    chats: Arc<ChatService>,
    models: Arc<ModelCatalogService>,
    stream: Arc<StreamService>,
    outbox: Arc<OutboxEnqueuer>,
}

/// Which mutation a commit performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Retry,
    Edit,
    Delete,
}

/// The replacement turn a retry/edit commit inserts.
#[derive(Clone)]
struct Replacement {
    turn: NewTurn,
    /// Text of the new user message.
    content: String,
    /// User message of the old turn whose live attachment links are copied.
    source_message_id: Option<Uuid>,
}

/// Everything the mutation transaction writes (cloned per transaction attempt).
#[derive(Clone)]
struct CommitPlan {
    kind: MutationKind,
    actor_user_id: Uuid,
    chat: chat::Model,
    old: chat_turn::Model,
    /// `(created_at, id)` of the old turn's user message (summary invalidation).
    user_msg_pos: MessagePosition,
    replacement: Option<Replacement>,
    now: DateTime<Utc>,
}

impl CommitPlan {
    fn audit_event(&self) -> TurnMutationAuditEvent {
        let tenant_id = self.chat.tenant_id;
        let chat_id = self.chat.id;
        let actor_user_id = self.actor_user_id;
        let original_request_id = self.old.request_id;
        let new_request_id = self
            .replacement
            .as_ref()
            .map_or(original_request_id, |r| r.turn.request_id);
        let timestamp = self.now.to_rfc3339_opts(SecondsFormat::AutoSi, true);
        match self.kind {
            MutationKind::Retry => TurnMutationAuditEvent::Retry {
                tenant_id,
                actor_user_id,
                chat_id,
                original_request_id,
                new_request_id,
                timestamp,
            },
            MutationKind::Edit => TurnMutationAuditEvent::Edit {
                tenant_id,
                actor_user_id,
                chat_id,
                original_request_id,
                new_request_id,
                timestamp,
            },
            MutationKind::Delete => TurnMutationAuditEvent::Delete {
                tenant_id,
                actor_user_id,
                chat_id,
                request_id: original_request_id,
                timestamp,
            },
        }
    }
}

/// `error_code` of a retry/edit turn whose setup failed after the commit
/// (DESIGN §5.7 "Unstarted retry/edit turn").
fn setup_failure_code(err: &DomainError) -> &'static str {
    match err {
        DomainError::ContextBudgetExceeded => error_codes::CONTEXT_LENGTH_EXCEEDED,
        DomainError::QuotaExceeded { .. } => error_codes::QUOTA_EXCEEDED,
        _ => error_codes::TURN_SETUP_FAILED,
    }
}

impl TurnService {
    #[must_use]
    pub fn new(deps: TurnDeps) -> Self {
        let TurnDeps {
            db,
            authz,
            chats,
            models,
            stream,
            outbox,
        } = deps;
        Self {
            db,
            authz,
            chats,
            models,
            stream,
            outbox,
        }
    }

    /// The non-deleted turn `request_id` of the caller's chat.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `TurnNotFound` (unknown or
    /// soft-deleted turn), database failures.
    pub async fn status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<chat_turn::Model> {
        let chat = self
            .authorized_chat(ctx, actions::READ_TURN, chat_id)
            .await?;
        TurnRepo::find_by_request(&self.db.conn()?, chat.tenant_id, chat.id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or(DomainError::TurnNotFound)
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`: replace the latest turn by
    /// a new one with the same user message and stream its answer.
    ///
    /// # Errors
    /// See [`Self::edit`] (without `EmptyContent`).
    pub async fn retry(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<LiveStream> {
        let chat = self
            .authorized_chat(ctx, actions::RETRY_TURN, chat_id)
            .await?;
        let old = self.mutation_target(ctx, &chat, request_id).await?;
        self.regenerate(ctx, chat, old, MutationKind::Retry, None)
            .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`: replace the latest turn by a new
    /// one with `content` as the user message and stream its answer.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `EmptyContent`, `TurnNotFound`,
    /// `NotLatestTurn`, `PermissionDenied` (another requester), `TurnNotTerminal`,
    /// `InvalidModel`, preflight rejections (nothing changed), `GenerationInProgress`
    /// (running-index race), setup failures after the commit (the new turn is
    /// failed), database failures.
    pub async fn edit(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> DomainResult<LiveStream> {
        let chat = self
            .authorized_chat(ctx, actions::EDIT_TURN, chat_id)
            .await?;
        if content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let old = self.mutation_target(ctx, &chat, request_id).await?;
        self.regenerate(ctx, chat, old, MutationKind::Edit, Some(content))
            .await
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`: soft-delete the latest turn and
    /// its messages.
    ///
    /// # Errors
    /// Authorization errors, `ChatNotFound`, `TurnNotFound`, `NotLatestTurn`,
    /// `PermissionDenied`, `TurnNotTerminal`, database failures.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<()> {
        let chat = self
            .authorized_chat(ctx, actions::DELETE_TURN, chat_id)
            .await?;
        let old = self.mutation_target(ctx, &chat, request_id).await?;
        let user_msg_pos = self.user_message_position(&chat, &old).await?.0;
        self.commit(CommitPlan {
            kind: MutationKind::Delete,
            actor_user_id: ctx.subject_id(),
            chat,
            old,
            user_msg_pos,
            replacement: None,
            now: now_utc(),
        })
        .await?;
        Ok(())
    }

    /// Authorize `action` on the chat (one PDP evaluation) and load it.
    async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        chat_id: Uuid,
    ) -> DomainResult<chat::Model> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        self.chats.load_chat(&scope, chat_id).await
    }

    /// Read-only mutation preview (DESIGN §3.9 rules 1–4, 9): the turn exists,
    /// is not deleted, belongs to the caller, is terminal and is the latest one.
    async fn mutation_target(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        request_id: Uuid,
    ) -> DomainResult<chat_turn::Model> {
        let conn = self.db.conn()?;
        let turn = TurnRepo::find_by_request(&conn, chat.tenant_id, chat.id, request_id)
            .await?
            .ok_or(DomainError::TurnNotFound)?;
        if turn.deleted_at.is_some() {
            return Err(DomainError::NotLatestTurn);
        }
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::PermissionDenied);
        }
        if turn.state == TurnState::Running.as_str() {
            return Err(DomainError::TurnNotTerminal);
        }
        let latest = TurnRepo::latest_live(&conn, chat.tenant_id, chat.id).await?;
        if latest.is_none_or(|l| l.id != turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        Ok(turn)
    }

    /// Position of the old turn's user message (falls back to the turn start when
    /// the message is missing) and the message itself.
    async fn user_message_position(
        &self,
        chat: &chat::Model,
        old: &chat_turn::Model,
    ) -> DomainResult<(MessagePosition, Option<message::Model>)> {
        let conn = self.db.conn()?;
        let msg = MessageRepo::user_message_of_turn(&conn, chat.tenant_id, chat.id, old.request_id)
            .await?;
        let pos = msg
            .as_ref()
            .map_or((old.started_at, Uuid::nil()), |m| (m.created_at, m.id));
        Ok((pos, msg))
    }

    /// Retry/edit after the preview: model, preflight, commit, setup, stream.
    async fn regenerate(
        &self,
        ctx: &SecurityContext,
        chat: chat::Model,
        old: chat_turn::Model,
        kind: MutationKind,
        new_content: Option<String>,
    ) -> DomainResult<LiveStream> {
        let user_id = ctx.subject_id();
        let model_id = chat.model.clone().ok_or(DomainError::InvalidModel)?;
        let model = self.models.resolve_chat_model(user_id, &model_id).await?;

        let (user_msg_pos, original) = self.user_message_position(&chat, &old).await?;
        let attachment_ids = match &original {
            Some(m) => {
                AttachmentRepo::live_linked_ids(&self.db.conn()?, chat.tenant_id, chat.id, m.id)
                    .await?
            }
            None => Vec::new(),
        };
        let content = match new_content {
            Some(c) => c,
            None => original
                .as_ref()
                .map(|m| m.content.clone())
                .ok_or_else(|| {
                    DomainError::internal(format!("turn {} has no user message", old.id))
                })?,
        };

        // Mutation preflight: a rejection changes nothing.
        let pre = self
            .stream
            .preflight(PreflightRequest {
                chat: &chat,
                user_id,
                model: &model,
                content: &content,
                attachment_ids: &attachment_ids,
                web_search: old.web_search_enabled,
            })
            .await?;

        // Mutation commit.
        let now = now_utc();
        let new_turn = NewTurn {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id: Uuid::new_v4(),
            requester_user_id: user_id,
            web_search_enabled: old.web_search_enabled,
            preflight: None,
            now,
        };
        let web_search = old.web_search_enabled;
        let turn = self
            .commit(CommitPlan {
                kind,
                actor_user_id: user_id,
                chat: chat.clone(),
                old,
                user_msg_pos,
                replacement: Some(Replacement {
                    turn: new_turn,
                    content: content.clone(),
                    source_message_id: original.map(|m| m.id),
                }),
                now,
            })
            .await?
            .ok_or_else(|| DomainError::internal("mutation commit returned no turn"))?;

        // Setup of the committed turn; a failure fails the turn (no reserve yet).
        let turn_id = turn.id;
        let prepared = self
            .stream
            .prepare_turn(TurnPrep {
                chat,
                user_id,
                content,
                request_id: turn.request_id,
                web_search_requested: web_search,
                pre,
                mode: TurnMode::Existing { turn },
            })
            .await;
        match prepared {
            Ok(prepared) => Ok(self.stream.spawn_turn(prepared)),
            Err(err) => {
                self.fail_unstarted(turn_id, &err).await;
                Err(err)
            }
        }
    }

    /// The mutation transaction (DESIGN §3.9 rule 7, "Summary Interaction on Turn
    /// Mutation", "Audit Events for Turn Mutations"). Returns the new running turn
    /// of a retry/edit.
    async fn commit(&self, plan: CommitPlan) -> DomainResult<Option<chat_turn::Model>> {
        let for_update = self.db.db().backend() == DbBackend::Postgres;
        let outbox = Arc::clone(&self.outbox);
        let (turn, wake) = with_tx_retry(&self.db, "turn mutation commit", move |tx| {
            let (plan, outbox) = (plan.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let tenant_id = plan.chat.tenant_id;
                let chat_id = plan.chat.id;
                let old = &plan.old;
                if for_update {
                    TurnRepo::lock_for_update(tx, tenant_id, chat_id, old.id).await?;
                }
                // First write: takes the write lock on SQLite, so the checks
                // below read the committed state of concurrent mutations.
                let replaced_by = plan.replacement.as_ref().map(|r| r.turn.request_id);
                if !TurnRepo::soft_delete(tx, old.id, replaced_by, plan.now).await? {
                    return Err(DomainError::NotLatestTurn);
                }
                let current = TurnRepo::find_by_request(tx, tenant_id, chat_id, old.request_id)
                    .await?
                    .ok_or(DomainError::NotLatestTurn)?;
                if current.state == TurnState::Running.as_str() {
                    return Err(DomainError::TurnNotTerminal);
                }
                if let Some(other) = TurnRepo::latest_live(tx, tenant_id, chat_id).await?
                    && (other.started_at, other.id) > (old.started_at, old.id)
                {
                    return Err(DomainError::NotLatestTurn);
                }
                MessageRepo::soft_delete_turn_messages(
                    tx,
                    tenant_id,
                    chat_id,
                    old.request_id,
                    plan.now,
                )
                .await?;
                ThreadSummaryRepo::invalidate_if_covers(
                    tx,
                    tenant_id,
                    chat_id,
                    plan.user_msg_pos.0,
                    plan.user_msg_pos.1,
                )
                .await?;

                let audit = MiniChatAuditEvent::Mutation(plan.audit_event());
                let turn = match plan.replacement {
                    Some(r) => {
                        let turn =
                            TurnRepo::insert_running(tx, r.turn)
                                .await
                                .map_err(|e| match e {
                                    DomainError::TurnAlreadyRunning => {
                                        DomainError::GenerationInProgress
                                    }
                                    other => other,
                                })?;
                        let message_id = Uuid::new_v4();
                        let row = user_message(&turn, message_id, r.content, plan.now);
                        if !MessageRepo::insert_if_absent(tx, row).await? {
                            return Err(DomainError::internal(format!(
                                "user message id {message_id} already exists"
                            )));
                        }
                        if let Some(source) = r.source_message_id {
                            let ids =
                                AttachmentRepo::live_linked_ids(tx, tenant_id, chat_id, source)
                                    .await?;
                            AttachmentRepo::link_to_message(
                                tx, tenant_id, chat_id, message_id, &ids, plan.now,
                            )
                            .await?;
                        }
                        ChatRepo::touch(tx, tenant_id, chat_id, plan.now).await?;
                        Some(turn)
                    }
                    None => None,
                };
                let wake = outbox
                    .enqueue_json(tx, QueueKind::Audit, tenant_id, AUDIT_PAYLOAD_TYPE, &audit)
                    .await?;
                Ok((turn, wake))
            })
        })
        .await?;
        wake.fire();
        Ok(turn)
    }

    /// Unstarted retry/edit turn (DESIGN §5.7): plain CAS to `failed`, no
    /// settlement and no outbox event (no reserve exists).
    async fn fail_unstarted(&self, turn_id: Uuid, err: &DomainError) {
        let update = TerminalUpdate {
            state: TurnState::Failed,
            error_code: Some(setup_failure_code(err).to_owned()),
            error_detail: None,
            assistant_message_id: None,
            provider_response_id: None,
            counters: TurnCounters::default(),
            now: now_utc(),
        };
        let res = match self.db.conn() {
            Ok(conn) => TurnRepo::cas_finalize(&conn, turn_id, &update).await,
            Err(e) => Err(e),
        };
        match res {
            Ok(true) => {}
            Ok(false) => {
                warn!(%turn_id, "retry/edit turn already left running before setup failure");
            }
            Err(e) => {
                warn!(%turn_id, error = %e, "failed to mark retry/edit turn failed after setup error");
            }
        }
    }
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod turn_tests;
