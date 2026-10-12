//! Retry, edit and delete of the latest turn (DESIGN section 3.9 "Turn Mutation Rules" and the
//! "Retry / edit variant" of section 3.6).
//!
//! The order never destroys the previous answer before the replacement can start: authorization,
//! the read-only mutation preview, the stream service's mutation preflight (quota cascade, kill
//! switches, image guards), then the mutation commit, and only then context assembly, the reserve
//! and the stream.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::model::{DeleteCommand, MutationOp, ReplaceCommand};
use super::ports::MutationRepository;
use crate::domain::authz::{Pep, actions};
use crate::domain::chat::ChatRepository;
use crate::domain::clock;
use crate::domain::error::{DomainError, ProtectedResource};
use crate::domain::metrics::Metrics;
use crate::domain::stream::{
    EventStream, MutationPreflight, NewTurnIds, StreamService, TurnRepository, secondary_image_ids,
};
use crate::domain::turn::{TurnRecord, TurnState};

/// The new user message of a replacement turn.
enum Replacement {
    /// The original content.
    Retry,
    /// The edited content (unchecked).
    Edit(String),
}

impl Replacement {
    fn op(&self) -> MutationOp {
        match self {
            Self::Retry => MutationOp::Retry,
            Self::Edit(_) => MutationOp::Edit,
        }
    }

    fn action(&self) -> &'static str {
        match self {
            Self::Retry => actions::RETRY_TURN,
            Self::Edit(_) => actions::EDIT_TURN,
        }
    }
}

/// The `result` label of the mutation metric for a failed mutation.
fn failure_label(err: &DomainError) -> &'static str {
    match err {
        DomainError::NotLatestTurn | DomainError::GenerationInProgress => "conflict",
        DomainError::Internal { .. }
        | DomainError::PolicyUnavailable { .. }
        | DomainError::AuthzUnavailable => "error",
        _ => "rejected",
    }
}

/// Collaborators of the mutation service.
pub struct TurnMutationServiceParts {
    /// The PEP.
    pub pep: Pep,
    /// Chat reads.
    pub chats: Arc<dyn ChatRepository>,
    /// Turn reads.
    pub turns: Arc<dyn TurnRepository>,
    /// Mutation persistence.
    pub mutations: Arc<dyn MutationRepository>,
    /// The stream pipeline a retry or edit continues in.
    pub stream: Arc<StreamService>,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
}

/// `POST /turns/{request_id}/retry`, `PATCH /turns/{request_id}` and
/// `DELETE /turns/{request_id}`.
#[domain_model]
pub struct TurnMutationService {
    pep: Pep,
    chats: Arc<dyn ChatRepository>,
    turns: Arc<dyn TurnRepository>,
    mutations: Arc<dyn MutationRepository>,
    stream: Arc<StreamService>,
    metrics: Arc<dyn Metrics>,
}

impl TurnMutationService {
    /// Creates the service.
    #[must_use]
    pub fn new(parts: TurnMutationServiceParts) -> Self {
        Self {
            pep: parts.pep,
            chats: parts.chats,
            turns: parts.turns,
            mutations: parts.mutations,
            stream: parts.stream,
            metrics: parts.metrics,
        }
    }

    /// Retries the latest turn: a new turn with the original content and attachments streams a
    /// new answer.
    ///
    /// # Errors
    /// The authorization, lookup, eligibility and preflight errors of the REST error table
    /// (nothing is changed), or a setup failure after the mutation commit (the new turn is marked
    /// `failed`).
    pub async fn retry(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<EventStream, DomainError> {
        self.measured(
            MutationOp::Retry,
            self.replace(ctx, chat_id, request_id, Replacement::Retry),
        )
        .await
    }

    /// Edits the latest turn: a new turn with `content` and the original attachments streams a new
    /// answer.
    ///
    /// # Errors
    /// As for [`Self::retry`], plus `EmptyContent`.
    pub async fn edit(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<EventStream, DomainError> {
        self.measured(
            MutationOp::Edit,
            self.replace(ctx, chat_id, request_id, Replacement::Edit(content)),
        )
        .await
    }

    /// Soft-deletes the latest turn; no new turn is created.
    ///
    /// # Errors
    /// The authorization, lookup and eligibility errors of the REST error table.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        self.measured(MutationOp::Delete, async {
            let scope = self
                .pep
                .chat_scope(ctx, actions::DELETE_TURN, Some(chat_id))
                .await?;
            self.require_chat(&scope, chat_id).await?;
            let turn = self.preview(&scope, ctx, chat_id, request_id).await?;
            self.mutations
                .delete(
                    &scope,
                    DeleteCommand {
                        tenant_id: ctx.subject_tenant_id(),
                        user_id: ctx.subject_id(),
                        chat_id,
                        request_id: turn.request_id,
                        now: clock::now(),
                    },
                )
                .await
        })
        .await
    }

    async fn replace(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        replacement: Replacement,
    ) -> Result<EventStream, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, replacement.action(), Some(chat_id))
            .await?;
        let chat = self.require_chat(&scope, chat_id).await?;
        let turn = self.preview(&scope, ctx, chat_id, request_id).await?;
        let original = self
            .mutations
            .original_message(&scope, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::internal("the turn to mutate has no user message"))?;
        let op = replacement.op();
        let content = match replacement {
            Replacement::Retry => original.content.clone(),
            Replacement::Edit(text) => {
                if text.trim().is_empty() {
                    return Err(DomainError::EmptyContent);
                }
                text
            }
        };
        let images: Vec<_> = original
            .attachments
            .iter()
            .filter(|attachment| attachment.kind == "image")
            .collect();
        let prepared = self
            .stream
            .mutation_preflight(
                ctx,
                &scope,
                &chat,
                MutationPreflight {
                    content: content.clone(),
                    web_search: turn.web_search_enabled,
                    image_ids: images
                        .iter()
                        .filter_map(|image| image.provider_file_id.clone())
                        .collect(),
                    image_secondary_ids: secondary_image_ids(&original.attachments),
                    image_count: images.len(),
                    replaced_request: request_id,
                },
            )
            .await?;
        let ids = NewTurnIds::generate();
        self.mutations
            .replace(
                &scope,
                ReplaceCommand {
                    op,
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    chat_id,
                    old_request_id: request_id,
                    new_request_id: ids.request_id,
                    new_turn_id: ids.turn_id,
                    new_user_message_id: ids.user_message_id,
                    content,
                    web_search_enabled: turn.web_search_enabled,
                    now: prepared.now(),
                },
            )
            .await?;
        self.stream
            .start_mutated(ctx, scope, chat, prepared, ids)
            .await
    }

    async fn require_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<crate::domain::chat::Chat, DomainError> {
        Ok(self
            .chats
            .find(scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })?
            .chat)
    }

    /// The read-only mutation preview: the turn exists in the chat, belongs to the caller, is in
    /// a terminal state (checked before the latest-turn check) and is the latest non-deleted
    /// turn.
    async fn preview(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnRecord, DomainError> {
        let turn = self
            .turns
            .find_by_request(scope, chat_id, request_id)
            .await?
            .ok_or(DomainError::TurnNotFound { request_id })?;
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AuthzDenied {
                resource: ProtectedResource::Chat,
            });
        }
        if turn.state == TurnState::Running {
            return Err(DomainError::InvalidTurnState);
        }
        if turn.deleted {
            return Err(DomainError::NotLatestTurn);
        }
        let latest = self.mutations.latest_turn_id(scope, chat_id).await?;
        if latest != Some(turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        Ok(turn)
    }

    /// Records the mutation metrics around `run`.
    async fn measured<T>(
        &self,
        op: MutationOp,
        run: impl Future<Output = Result<T, DomainError>>,
    ) -> Result<T, DomainError> {
        let started = Instant::now();
        let result = run.await;
        let label = match &result {
            Ok(_) => "ok",
            Err(err) => failure_label(err),
        };
        self.metrics.turn_mutation(op.as_str(), label);
        self.metrics
            .turn_mutation_latency_ms(op.as_str(), started.elapsed().as_secs_f64() * 1000.0);
        result
    }
}
