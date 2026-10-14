//! The turn mutation service (retry, edit, delete) and the reaction service. Every step of the
//! mutation flow is a public method, so a test can interleave two mutations between the
//! preview, the preflight and the commit.

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Operation, PepService};
use crate::domain::chats::{ChatDetail, scope_of};
use crate::domain::error::{DomainError, QuotaScope, ResourceKind};
use crate::domain::messages::{MessageRow, Reaction};
use crate::domain::mutations::carry::CarriedAttachments;
use crate::domain::mutations::metrics::MutationMetrics;
use crate::domain::mutations::reactions::StoredReaction;
use crate::domain::mutations::request::ValidatedEdit;
use crate::domain::mutations::store::{
    CommitCommand, ExistingReserve, MutationStore, MutationTurn, ReactionStore, Replacement,
    UnstartedFailure,
};
use crate::domain::mutations::{
    MutationOp, MutationResult, carry_forward_attachments, check_latest, check_reaction_target,
    check_target, classify_rejection,
};
use crate::domain::quota::{
    Clock, PreflightDecision, PreflightRequest, RequestFacts, ReserveReceipt,
};
use crate::domain::streaming::{
    MessageStreamService, ProviderCallPlan, ReplacementRequest, ReservedTurn, SendOutcome,
    SnapshotBoundary, TurnOutbox, check_image_guards,
};

/// The result of the read-only mutation preview.
#[domain_model]
#[derive(Debug, Clone)]
pub struct MutationPreview {
    pub op: MutationOp,
    pub target: MutationTurn,
    /// The target's non-deleted user message.
    pub user_message: MessageRow,
    /// The start of the preview (the mutation latency series starts here).
    pub started: Instant,
}

/// The result of the mutation preflight: everything the commit and the post-commit setup need.
#[domain_model]
#[derive(Debug, Clone)]
pub struct PreparedMutation {
    pub preview: MutationPreview,
    /// The content of the replacement message.
    pub content: String,
    /// The decision with its preflight instant (`started_at`).
    pub decision: PreflightDecision,
    /// The catalog entry of the effective model.
    pub model: ModelCatalogEntry,
    pub carried: CarriedAttachments,
    /// The target turn's `web_search_enabled`.
    pub web_search_enabled: bool,
    /// The snapshot boundary: the latest non-deleted message of another turn.
    pub boundary: Option<SnapshotBoundary>,
}

/// What a committed mutation leaves for the post-commit steps.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedMutation {
    pub op: MutationOp,
    pub target_request_id: Uuid,
    /// The replacement turn of a retry or edit.
    pub replacement: Option<CommittedReplacement>,
    /// Whether the summary invalidation deleted the thread summary.
    pub summary_invalidated: bool,
}

/// The replacement turn a retry or edit committed.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedReplacement {
    pub turn_id: Uuid,
    /// The server-generated `request_id`.
    pub request_id: Uuid,
    pub user_message_id: Uuid,
    /// The `created_at` written on the replacement user message.
    pub user_message_created_at: OffsetDateTime,
}

/// The collaborators of the mutation service.
#[domain_model]
pub struct MutationDeps {
    /// The send pipeline the post-commit setup reuses (PEP, policy gateway, quota, providers,
    /// provider task, finalizer, settings).
    pub streaming: Arc<MessageStreamService>,
    pub store: Arc<dyn MutationStore>,
    /// The audit queue of the mutation transaction.
    pub outbox: Arc<dyn TurnOutbox>,
    pub metrics: Arc<dyn MutationMetrics>,
}

/// Retry, edit and delete of the latest terminal turn.
#[domain_model]
pub struct TurnMutationService {
    pub deps: MutationDeps,
}

fn internal(text: &str) -> DomainError {
    DomainError::internal(text)
}

impl TurnMutationService {
    /// Create the service over its collaborators.
    #[must_use]
    pub fn new(deps: MutationDeps) -> Self {
        Self { deps }
    }

    fn record_rejection(&self, op: MutationOp, error: &DomainError, at_preflight: bool) {
        self.deps
            .metrics
            .mutation(op, classify_rejection(error, at_preflight));
    }

    /// The SSE ping interval of the replacement stream (the send pipeline's setting).
    #[must_use]
    pub fn ping_interval(&self) -> std::time::Duration {
        self.deps.streaming.settings.sse_ping_interval
    }

    /// Admission: the PDP is evaluated once with the action of `op` and the chat id, then the
    /// owner-scoped chat load.
    ///
    /// # Errors
    ///
    /// An admission refusal or `NotFound { Chat }`.
    pub async fn admit(
        &self,
        ctx: &SecurityContext,
        op: MutationOp,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-refusal
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return-refusal
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-refusal
        // the PDP is evaluated once, with the action of the operation and the chat id as
        // `resource.id`; the mutation, the preflight and the stream reuse the scope. A refusal
        // (401, 403, 503) or a chat that is not visible (404) ends the request before any write
        let admission = self
            .deps
            .streaming
            .pep
            .authorize(ctx, op.operation(), Some(chat_id))
            .await;
        let scope = scope_of(admission?)?;
        self.deps.store.load_chat(&scope, chat_id).await
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-refused
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-admission
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-refused
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-admission
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-refused
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-admission
    }

    /// The read-only mutation preview: target lookup, requester, soft-delete, running and
    /// latest-turn checks, then the target's user message. Writes nothing and takes no lock.
    ///
    /// # Errors
    ///
    /// `NotFound { Turn }`, `AuthzDenied`, `Conflict { NotLatestTurn }`, `TurnNotTerminal`,
    /// `Internal`.
    pub async fn preview(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        op: MutationOp,
        request_id: Uuid,
    ) -> Result<MutationPreview, DomainError> {
        let started = Instant::now();
        let tenant_id = ctx.subject_tenant_id();
        // the lookup includes a soft-deleted row; the missing, requester, soft-deleted and
        // running checks run in the normative order (`check_target`)
        let target = self
            .deps
            .store
            .find_mutation_turn(tenant_id, chat.id, request_id)
            .await?;
        check_target(ctx.subject_id(), target.as_ref().map(|turn| &turn.row))?;
        let target = target.ok_or_else(|| internal("the checked target vanished"))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-latest
        let latest = self.deps.store.latest_turn(tenant_id, chat.id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-latest
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-generic-detail
        // the 409 carries a generic detail (the error mapping); the internal cause is logged
        check_latest(&target.row, latest.as_ref()).inspect_err(|_| {
            tracing::info!(
                chat_id = %chat.id,
                target_turn = %target.row.id,
                latest_turn = ?latest.as_ref().map(|turn| turn.id),
                "the target is not the latest turn of the chat"
            );
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-generic-detail
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-select-user-message
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-user-message-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-500
        let user_message = self
            .deps
            .store
            .target_user_message(tenant_id, chat.id, request_id)
            .await?
            .ok_or_else(|| {
                tracing::error!(chat_id = %chat.id, %request_id, "the target turn has no user message");
                internal("the target turn has no user message")
            })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return-500
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-user-message-missing
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-select-user-message
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return
        Ok(MutationPreview {
            op,
            target,
            user_message,
            started,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview:p1:inst-mp-return
    }

    /// The mutation preflight (retry and edit): model, carry-forward, snapshot boundary, image
    /// count, quota preflight with the caller as the requester, input limit and image guards.
    /// Writes nothing, reserves nothing and calls no provider.
    ///
    /// # Errors
    ///
    /// Every preflight rejection of the send pipeline.
    pub async fn preflight(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        preview: MutationPreview,
        content: String,
    ) -> Result<PreparedMutation, DomainError> {
        let streaming = &self.deps.streaming;
        let user_id = ctx.subject_id();
        let tenant_id = ctx.subject_tenant_id();
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-resolve-model
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-policy-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-policy-500
        // the policy snapshot is asked on every request; a plugin failure is a generic 500
        let snapshot = streaming.gateway.current_snapshot(user_id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-policy-500
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-policy-failed
        // the chat's model, without the enabled filter
        let model_known = snapshot
            .model_catalog
            .iter()
            .any(|entry| entry.id == chat.model);
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-resolve-model
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-model-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-invalid-model
        if !model_known {
            return Err(DomainError::InvalidModel {
                model: chat.model.clone(),
            });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-invalid-model
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-model-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-snapshot-boundary
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-read-facts
        // the snapshot boundary (computed once, immutable for the request), the links of the
        // original user message and the chat facts of the quota estimate
        let reads = self
            .deps
            .store
            .read_mutation(tenant_id, chat.id, preview.target.row.request_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-read-facts
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-snapshot-boundary
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-carry-forward
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-no-revalidation
        // the links were validated when the original message was sent: the `attachment_ids`
        // validation of the send pipeline does not run again
        let carried = carry_forward_attachments(reads.linked);
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-no-revalidation
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-web-search-flag
        // the client cannot change the web search flag on retry or edit
        let web_search_enabled = preview.target.web_search_enabled;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-web-search-flag
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-carry-forward
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-too-many-images
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-too-many-images
        let image_count = u32::try_from(carried.images.len()).unwrap_or(u32::MAX);
        if image_count > streaming.settings.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-too-many-images
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-too-many-images
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-quota-preflight
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-quota-rejected
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-quota
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-limits
        // the quota preflight of the requester (the caller is charged), with the daily web
        // search and code interpreter checks and the per-message limits of a new send; its
        // rejection (400 `FEATURE_DISABLED`, 429, 500) is returned unchanged
        let decision = streaming
            .quota
            .preflight(
                ctx,
                &PreflightRequest {
                    chat_id: chat.id,
                    selected_model: chat.model.clone(),
                    facts: RequestFacts {
                        message_bytes: content.len() as u64,
                        image_count,
                        ready_documents: reads.ready_documents,
                        ready_xlsx_attachments: reads.ready_code_interpreter,
                        web_search_enabled,
                    },
                },
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-limits
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-quota
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-quota-rejected
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-quota-preflight
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-input-too-long
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-input-too-long
        if decision.input_too_long() {
            return Err(DomainError::InputTooLong);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return-input-too-long
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-input-too-long
        // the effective model of the decision, read from the snapshot of the version the
        // preflight bound (asked again when the policy moved in between)
        let snapshot = if snapshot.policy_version == decision.policy_version_applied {
            snapshot
        } else {
            streaming.gateway.current_snapshot(user_id).await?
        };
        let model = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == decision.effective_model)
            .cloned()
            .ok_or_else(|| internal("the effective model is missing from the catalog"))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-image-guards
        // images are never dropped to make the request pass
        check_image_guards(image_count, &snapshot.kill_switches, decision.vision_input)?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-image-guards
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-keep-decision
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-return
        // the decision, with its preflight instant and period starts, is kept for the commit and
        // the reserve write; no row was written, no reserve taken and no provider request sent
        Ok(PreparedMutation {
            preview,
            content,
            decision,
            model,
            carried,
            web_search_enabled,
            boundary: reads.boundary,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-return
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-return
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight:p1:inst-mpf-keep-decision
    }

    /// The mutation commit transaction, with a server-generated `request_id` for a retry or
    /// edit; the outbox wake and the mutation metrics follow the commit.
    ///
    /// # Errors
    ///
    /// `Conflict { NotLatestTurn }`, `Conflict { GenerationInProgress }`, `TurnNotTerminal`,
    /// `OutboxPayloadTooLarge`, `Internal`; nothing is left behind.
    pub async fn commit(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        prepared: &PreparedMutation,
    ) -> Result<CommittedMutation, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-new-request-id
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-new-request-id
        // the `request_id` of the replacement turn is a server-generated UUID v4; the client
        // never supplies it
        let replacement = Replacement {
            request_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            content: prepared.content.clone(),
            attachment_ids: prepared.carried.attachment_ids.clone(),
            web_search_enabled: prepared.web_search_enabled,
            started_at: prepared.decision.started_at,
        };
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-new-request-id
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-new-request-id
        let command = CommitCommand {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            op: prepared.preview.op,
            target_request_id: prepared.preview.target.row.request_id,
            replacement: Some(replacement),
            now: self.deps.streaming.quota.clock.now(),
        };
        self.commit_command(&command, prepared.preview.started)
            .await
    }

    /// The delete commit (no preflight): the mutation transaction without a replacement.
    ///
    /// # Errors
    ///
    /// As [`commit`](Self::commit).
    pub async fn commit_delete(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        preview: &MutationPreview,
    ) -> Result<CommittedMutation, DomainError> {
        let command = CommitCommand {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            op: MutationOp::Delete,
            target_request_id: preview.target.row.request_id,
            replacement: None,
            now: self.deps.streaming.quota.clock.now(),
        };
        self.commit_command(&command, preview.started).await
    }

    async fn commit_command(
        &self,
        command: &CommitCommand,
        started: Instant,
    ) -> Result<CommittedMutation, DomainError> {
        let committed = self
            .deps
            .store
            .commit_mutation(Arc::clone(&self.deps.outbox), command)
            .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-latency
        // a committed mutation is `ok` (a later setup failure of the replacement turn is
        // reported through its turn state); the latency runs from the start of the preview
        self.deps.metrics.mutation(command.op, MutationResult::Ok);
        self.deps
            .metrics
            .mutation_latency(command.op, started.elapsed());
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-latency
        Ok(committed)
    }

    /// The unstarted replacement turn failure: a plain CAS to `failed`; a failure of the update
    /// is logged and left to the orphan watchdog.
    async fn fail_unstarted(&self, tenant_id: Uuid, turn_id: Uuid, code: UnstartedFailure) {
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-failed-setup
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-return
        match self
            .deps
            .store
            .fail_unstarted(tenant_id, turn_id, code)
            .await
        {
            Ok(true) => {}
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-cas-lost
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-return-cas-lost
            Ok(false) => {
                tracing::info!(%turn_id, "another finalizer already ended the replacement turn");
            }
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-return-cas-lost
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-cas-lost
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-update-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-orphan-fallback
            Err(error) => tracing::error!(
                %turn_id, %error,
                "the unstarted replacement turn could not be failed; the orphan watchdog finalizes it"
            ),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-orphan-fallback
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-update-failed
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-return
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-failed-setup
    }

    /// Mark the replacement turn `failed` after a setup failure and log it (the chat id, the
    /// original and the new `request_id` and the error code, never content); the error is the
    /// JSON Problem the caller returns.
    async fn fail_setup(
        &self,
        tenant_id: Uuid,
        turn: &SetupTurn,
        code: UnstartedFailure,
        error: DomainError,
    ) -> DomainError {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-logs
        tracing::warn!(
            chat_id = %turn.chat,
            original_request_id = %turn.original,
            new_request_id = %turn.replacement,
            error_code = code.as_str(),
            %error,
            "the replacement turn setup failed"
        );
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability:p1:inst-obs-logs
        self.fail_unstarted(tenant_id, turn.turn, code).await;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-keep-replacement
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-not-sse
        // the replaced turn stays replaced and the replacement user message stays in place; the
        // setup codes are never sent over SSE: the caller returns the JSON Problem of `error`
        // and the turn status reports the stored code
        error
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-not-sse
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-keep-replacement
    }

    /// The post-commit setup of a replacement turn: tool and context assembly, provider
    /// resolution, the `Existing` reserve and the hand-over to the send pipeline; a failure
    /// marks the replacement turn `failed` first.
    ///
    /// # Errors
    ///
    /// `ContextBudgetExceeded`, `QuotaExceeded { Tokens }`, `Internal`.
    pub async fn setup(
        &self,
        ctx: &SecurityContext,
        chat: &ChatDetail,
        prepared: PreparedMutation,
        committed: &CommittedMutation,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        let streaming = &self.deps.streaming;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-no-pdp
        // no PDP call: every query is tenant-scoped on the chat authorized by the admission of
        // the mutation
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-no-pdp
        let replacement = committed
            .replacement
            .as_ref()
            .ok_or_else(|| internal("a delete has no replacement turn"))?;
        let turn = SetupTurn {
            chat: chat.id,
            original: prepared.preview.target.row.request_id,
            replacement: replacement.request_id,
            turn: replacement.turn_id,
        };
        let assembly = match streaming
            .assemble_replacement(
                ctx,
                chat,
                &ReplacementRequest {
                    request_id: replacement.request_id,
                    content: &prepared.content,
                    carried_images: &prepared.carried.images,
                    web_search_enabled: prepared.web_search_enabled,
                    decision: &prepared.decision,
                    model: &prepared.model,
                    boundary: prepared.boundary.as_ref(),
                },
            )
            .await
        {
            Ok(assembly) => assembly,
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-context-rejected
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-context
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-provider-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-provider
            // a budget rejection is `context_length_exceeded` (400 `CONTEXT_BUDGET_EXCEEDED`),
            // an unresolvable provider or any other assembly failure is `turn_setup_failed` (500)
            Err(error) => {
                let code = if matches!(error, DomainError::ContextBudgetExceeded) {
                    UnstartedFailure::ContextLengthExceeded
                } else {
                    UnstartedFailure::TurnSetupFailed
                };
                return Err(self.fail_setup(tenant_id, &turn, code, error).await);
            } // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-provider
              // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-provider-failed
              // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-context
              // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-context-rejected
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-message-id
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-assistant
        // no assistant row exists until finalization, where the send pipeline stamps it strictly
        // after its user message
        let assistant_message_id = Uuid::new_v4();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-assistant
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-message-id
        let receipt = self
            .reserve(tenant_id, user_id, &prepared, &turn, streaming)
            .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-commit
        receipt.record_after_commit(streaming.quota.metrics.as_ref());
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-commit
        let plan = ProviderCallPlan {
            tenant_id,
            user_id,
            chat_id: chat.id,
            request_id: replacement.request_id,
            turn_id: replacement.turn_id,
            user_message_id: replacement.user_message_id,
            assistant_message_id,
            selected_model: chat.model.clone(),
            model: prepared.model.clone(),
            decision: prepared.decision.clone(),
            receipt: receipt.clone(),
            context: assembly.context,
            tools: assembly.tools,
            provider: assembly.provider,
            web_search_enabled: prepared.web_search_enabled,
            started_at: prepared.decision.started_at,
        };
        let reserved = ReservedTurn {
            turn_id: replacement.turn_id,
            user_message_id: replacement.user_message_id,
            user_message_created_at: replacement.user_message_created_at,
            started_at: prepared.decision.started_at,
            receipt,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-before-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-handoff
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-dropped
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-dropped
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-dropped
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-finalized
        // the provider request is allowed only after the reserve commit above; the reserved turn
        // goes to the send pipeline unchanged (a token cancelled after the reserve finalizes the
        // turn as `cancelled` there, without any SSE event, and the finalization with
        // `Finalizer::Stream` is the only settlement and outbox path of retry and edit)
        streaming.hand_over(plan, reserved, cancel).await
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-finalized
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-dropped
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-dropped
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-dropped
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-handoff
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-before-provider
    }

    /// The reserve transaction of the replacement turn through the `Existing` path; a failure
    /// marks the turn `failed` (`quota_exceeded` for the re-check, `turn_setup_failed` otherwise).
    async fn reserve(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        prepared: &PreparedMutation,
        turn: &SetupTurn,
        streaming: &MessageStreamService,
    ) -> Result<ReserveReceipt, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-reserve
        // the decision, the turn identity and `ReserveTurn::Existing`: the preflight columns
        // are filled once, `started_at` is the preflight instant and the limits are re-checked
        let reserved = self
            .deps
            .store
            .reserve_existing(
                &streaming.quota,
                &ExistingReserve {
                    tenant_id,
                    user_id,
                    chat_id: turn.chat,
                    request_id: turn.replacement,
                    turn_id: turn.turn,
                    decision: prepared.decision.clone(),
                },
            )
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-reserve
        let error = match reserved {
            Ok(receipt) => return Ok(receipt),
            Err(error) => error,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-over-limit
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-429
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-reserve-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-reserve-500
        // the reserve transaction rolled back; an over-limit bucket is 429 scope `tokens` without
        // any downgrade, every other failure is a generic 500
        let code = if matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ) {
            UnstartedFailure::QuotaExceeded
        } else {
            UnstartedFailure::TurnSetupFailed
        };
        Err(self.fail_setup(tenant_id, turn, code, error).await)
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-reserve-500
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-reserve-failed
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-return-429
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup:p1:inst-pcs-over-limit
    }

    /// The whole flow of a retry or an edit.
    async fn replace(
        &self,
        ctx: &SecurityContext,
        op: MutationOp,
        chat_id: Uuid,
        request_id: Uuid,
        edited: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        let chat = self
            .admit(ctx, op, chat_id)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-no-early-lock
        // any number of requests pass the preview and the preflight concurrently: neither holds
        // a lock or writes a row, so a loser has nothing to undo
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preview
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preview-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-preview
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-preview
        let preview = self
            .preview(ctx, &chat, op, request_id)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-preview
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-preview
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preview-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preview
        // a retry replays the original content, an edit carries the validated one
        let content = edited.unwrap_or_else(|| preview.user_message.content.clone());
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preflight
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preflight-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-preflight
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-preflight
        let prepared = self
            .preflight(ctx, &chat, preview, content)
            .await
            .inspect_err(|error| self.record_rejection(op, error, true))?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-preflight
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-preflight
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preflight-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-preflight
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution:p1:inst-cr-no-early-lock
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-commit
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-commit-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-commit
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-commit
        let committed = self
            .commit(ctx, &chat, &prepared)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-commit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-commit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-commit-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-commit
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-setup
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-setup-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-setup
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-setup
        self.setup(ctx, &chat, prepared, &committed, cancel).await
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-setup
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-setup
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-setup-failed
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-setup
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    ///
    /// # Errors
    ///
    /// Every rejection before the commit, at the commit and of the post-commit setup.
    pub async fn retry(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        self.replace(ctx, MutationOp::Retry, chat_id, request_id, None, cancel)
            .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// As [`retry`](Self::retry).
    pub async fn edit(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        edit: ValidatedEdit,
        cancel: &CancellationToken,
    ) -> Result<SendOutcome, DomainError> {
        self.replace(
            ctx,
            MutationOp::Edit,
            chat_id,
            request_id,
            Some(edit.content),
            cancel,
        )
        .await
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// Every rejection of the preview and of the commit.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let op = MutationOp::Delete;
        let chat = self
            .admit(ctx, op, chat_id)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-preview
        let preview = self
            .preview(ctx, &chat, op, request_id)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-preview
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-commit
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-commit-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-commit
        // no quota preflight, no reserve and no provider call take place
        self.commit_delete(ctx, &chat, &preview)
            .await
            .inspect_err(|error| self.record_rejection(op, error, false))?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-commit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-commit-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-commit
        Ok(())
    }
}

/// The identity of a replacement turn for the logs of a setup failure.
struct SetupTurn {
    chat: Uuid,
    /// The `request_id` of the replaced turn.
    original: Uuid,
    /// The server-generated `request_id` of the replacement turn.
    replacement: Uuid,
    /// The id of the replacement turn row.
    turn: Uuid,
}

/// `PUT` and `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.
#[domain_model]
pub struct ReactionService {
    pub pep: Arc<PepService>,
    pub store: Arc<dyn ReactionStore>,
    pub clock: Arc<dyn Clock>,
}

impl ReactionService {
    /// Create the service.
    #[must_use]
    pub fn new(pep: Arc<PepService>, store: Arc<dyn ReactionStore>, clock: Arc<dyn Clock>) -> Self {
        Self { pep, store, clock }
    }

    /// The admission, the owner-scoped chat load and the target resolution.
    async fn target(
        &self,
        ctx: &SecurityContext,
        operation: Operation,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-refusal
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-return-refusal
        // the action (`set_reaction` or `delete_reaction`) and the chat id as `resource.id`; a
        // refusal (401, 403, 503) or a chat that is not visible (404) ends the request
        let admission = self.pep.authorize(ctx, operation, Some(chat_id)).await;
        let scope = scope_of(admission?)?;
        let chat = self.store.load_chat(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-refused
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-admission
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-refused
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-admission
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-select
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return-404
        // a non-deleted message of the scoped chat and the tenant: an unknown id, a message of
        // another chat and a soft-deleted message are the same 404 with the message type
        let message = self
            .store
            .load_message(ctx.subject_tenant_id(), chat.id, message_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: ResourceKind::Message,
                id: Some(message_id.to_string()),
            })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return-404
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-missing
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-select
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return
        check_reaction_target(&message.role)
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target:p1:inst-rt-return
    }

    /// Set (upsert) the caller's reaction on an assistant message of the chat.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { Chat }`, `NotFound { Message }`,
    /// `ReactionTargetNotAssistant`, `Internal`.
    pub async fn set(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: Reaction,
    ) -> Result<StoredReaction, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-target
        self.target(ctx, Operation::SetReaction, chat_id, message_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-target
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-upsert
        self.store
            .upsert_reaction(
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                message_id,
                reaction,
                self.clock.now(),
            )
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-upsert
    }

    /// Remove the caller's reaction (idempotent).
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    pub async fn remove(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-target
        self.target(ctx, Operation::DeleteReaction, chat_id, message_id)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-target
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-remove
        self.store
            .remove_reaction(ctx.subject_tenant_id(), ctx.subject_id(), message_id)
            .await
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-remove
    }
}
