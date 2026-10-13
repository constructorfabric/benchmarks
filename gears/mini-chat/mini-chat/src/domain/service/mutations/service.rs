//! The mutation service: retry, edit and delete of the latest terminal turn
//! (`cpt-cf-mini-chat-flow-turn-mutations-retry-turn`,
//! `cpt-cf-mini-chat-flow-turn-mutations-edit-turn`,
//! `cpt-cf-mini-chat-flow-turn-mutations-delete-turn`).

use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::time::Instant;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::audit::{AuditInput, mutation_audit_event};
use super::history::load_replacement_history;
use super::inputs::load_mutation_inputs;
use super::ports::{DeletePlan, MutationMetrics, MutationRepository, ReplacementPlan};
use super::preview::preview;
use super::setup::{FailedSetup, SetupGuard, fail_setup};
use super::timing::mutation_time;
use super::types::{MutationOp, MutationResult, MutationTarget, SetupFailureCode};
use crate::domain::authz::{AuthzService, ChatAction};
use crate::domain::error::{DisabledFeature, DomainError};
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::quota::preflight::{PreflightDecision, PreflightRequest};
use crate::domain::service::quota::service::{Clock, QuotaService};
use crate::domain::service::stream::context::check_input_length;
use crate::domain::service::stream::inputs::TurnInputs;
use crate::domain::service::stream::pipeline::{ReplacementStart, SendOutcome, StreamService};
use crate::domain::service::stream::ports::{MessageBoundary, TurnRepository};
use crate::domain::service::stream::relay::RelayHandle;
use crate::domain::service::stream::types::StreamStarted;

/// Collaborators of the mutation service.
pub struct MutationDeps {
    /// PEP (`retry_turn`, `edit_turn`, `delete_turn`).
    pub authz: Arc<AuthzService>,
    /// Chat store (scoped chat load).
    pub chats: Arc<dyn ChatRepository>,
    /// Model-policy gateway (catalog check).
    pub policy: Arc<dyn PolicyCatalog>,
    /// Quota service (preflight and the fill-mode reserve).
    pub quota: Arc<QuotaService>,
    /// Turn store of the stream pipeline (attachments, summary and recent
    /// messages, the running-turn re-query).
    pub turns: Arc<dyn TurnRepository>,
    /// Mutation store.
    pub mutations: Arc<dyn MutationRepository>,
    /// The stream service: the tool decision, the context assembly, the
    /// provider resolution and the hand-off to the provider task.
    pub stream: Arc<StreamService>,
    /// Mutation metrics sink.
    pub metrics: Arc<dyn MutationMetrics>,
    /// Clock of the timestamps the service writes.
    pub clock: Clock,
}

/// The mutation service.
pub struct MutationService {
    deps: MutationDeps,
}

/// A failed replacement, by the side of the commit it happened on.
enum Failure {
    /// Before the commit: the previous turn stays.
    BeforeCommit(DomainError),
    /// After the commit: the new turn was marked failed.
    AfterCommit(DomainError),
}

impl From<DomainError> for Failure {
    fn from(error: DomainError) -> Self {
        Self::BeforeCommit(error)
    }
}

/// The request of a retry or an edit.
struct ReplaceRequest {
    op: MutationOp,
    ctx: SecurityContext,
    chat_id: Uuid,
    request_id: Uuid,
    /// The edited content; `None` for a retry.
    content: Option<String>,
    accepted_at: Instant,
}

type Handoff = (SendOutcome, oneshot::Sender<()>);

impl MutationService {
    /// Creates the service.
    #[must_use]
    pub fn new(deps: MutationDeps) -> Self {
        Self { deps }
    }

    /// `POST /v1/chats/{id}/turns/{request_id}/retry`: runs the whole pipeline
    /// in a separately spawned task and awaits only its result.
    ///
    /// # Errors
    ///
    /// Every pre-stream refusal as its [`DomainError`].
    pub async fn retry(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<SendOutcome, DomainError> {
        self.spawn_replacement(ReplaceRequest {
            op: MutationOp::Retry,
            ctx,
            chat_id,
            request_id,
            content: None,
            accepted_at: Instant::now(),
        })
        .await
    }

    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// `EmptyContent` (before authorization), then as [`Self::retry`].
    pub async fn edit(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<SendOutcome, DomainError> {
        self.spawn_replacement(ReplaceRequest {
            op: MutationOp::Edit,
            ctx,
            chat_id,
            request_id,
            content: Some(content),
            accepted_at: Instant::now(),
        })
        .await
    }

    async fn spawn_replacement(
        self: &Arc<Self>,
        request: ReplaceRequest,
    ) -> Result<SendOutcome, DomainError> {
        let (result_tx, result_rx) = oneshot::channel();
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-spawn-retry-setup
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-spawn-edit-setup
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match this.replace(request).await {
                Ok((outcome, go)) => {
                    // A receiver that is gone returns the outcome: dropping its
                    // relay handle cancels the token before the go-signal.
                    if let Err(undelivered) = result_tx.send(Ok(outcome)) {
                        drop(undelivered);
                    }
                    if go.send(()).is_err() {
                        tracing::debug!("the provider task ended before its go-signal");
                    }
                }
                Err(error) => {
                    if result_tx.send(Err(error)).is_err() {
                        tracing::debug!("the handler is gone, the mutation error is dropped");
                    }
                }
            }
        });
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-spawn-edit-setup
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-spawn-retry-setup
        result_rx.await.unwrap_or_else(|_| {
            Err(DomainError::Internal(
                "the mutation setup ended without a result".to_owned(),
            ))
        })
    }

    /// Runs a retry or an edit and records `mini_chat_turn_mutation` once.
    async fn replace(&self, request: ReplaceRequest) -> Result<Handoff, DomainError> {
        let op = request.op;
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-run-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-run-edit
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation
        match self.replace_inner(request).await {
            Ok(handoff) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-record-ok
                self.deps.metrics.turn_mutation(op, MutationResult::Ok);
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-record-ok
                Ok(handoff)
            }
            Err(Failure::BeforeCommit(error)) => {
                self.deps
                    .metrics
                    .turn_mutation(op, MutationResult::before_commit(&error));
                Err(error)
            }
            Err(Failure::AfterCommit(error)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-record-setup-failed
                self.deps
                    .metrics
                    .turn_mutation(op, MutationResult::SetupFailed);
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-fail-setup:p1:inst-record-setup-failed
                Err(error)
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-run-edit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-run-retry
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-replacement-pipeline:p1
    async fn replace_inner(&self, request: ReplaceRequest) -> Result<Handoff, Failure> {
        let deps = &self.deps;
        let ReplaceRequest {
            op,
            ctx,
            chat_id,
            request_id,
            content,
            accepted_at,
        } = request;
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-validate-edit
        if content
            .as_deref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(DomainError::EmptyContent.into());
        }
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-validate-edit
        let action = if op == MutationOp::Edit {
            ChatAction::EditTurn
        } else {
            ChatAction::RetryTurn
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-authorize-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-authorize-edit
        let scope = deps.authz.chat_scope(&ctx, action, Some(chat_id)).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-authorize-edit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-authorize-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-load-chat-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-load-chat-edit
        let chat = deps.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-load-chat-edit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-load-chat-retry
        let user_id = ctx.subject_id();
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-preview-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-preview-edit
        let target = preview(&*deps.mutations, &scope, chat.id, user_id, request_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-preview-edit
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-preview-retry
        let content = content.unwrap_or_else(|| target.user_message.content.clone());

        let checked = self
            .check_replacement(&scope, &chat, user_id, &target, &content)
            .await?;
        let plan = self.replacement_plan(op, &chat, user_id, &target, content.clone())?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-replace
        self.commit_replacement(&scope, &plan).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-replace
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-record-commit-latency
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation-latency
        deps.metrics
            .turn_mutation_latency_ms(op, accepted_at.elapsed().as_secs_f64() * 1000.0);
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-record-metrics:p1:inst-metric-mutation-latency
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-record-commit-latency

        let marked = FailedSetup {
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            turn_id: plan.new_turn_id,
            request_id: plan.new_request_id,
            code: SetupFailureCode::TurnSetupFailed,
            at: plan.now,
        };
        let guard = SetupGuard::arm(
            Arc::clone(&deps.mutations),
            Arc::clone(&deps.metrics),
            op,
            marked,
        );
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-try-setup
        let started = self
            .start_replacement(ReplacementSetup {
                ctx,
                scope: &scope,
                chat: &chat,
                user_id,
                plan: &plan,
                decision: checked.decision,
                inputs: checked.inputs,
                content: &content,
                accepted_at,
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-try-setup
        guard.disarm();
        match started {
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-return-replacement
            Ok((started, relay, go)) => Ok((SendOutcome::Stream { started, relay }, go)),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-return-replacement
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-catch-setup
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mark-setup-failed
                // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-failed-setup
                fail_setup(
                    &*deps.mutations,
                    &FailedSetup {
                        code: SetupFailureCode::of_error(&error),
                        at: (deps.clock)(),
                        ..marked
                    },
                )
                .await;
                // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-replacement-setup:p1:inst-to-failed-setup
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mark-setup-failed
                // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-return-setup-failed
                Err(Failure::AfterCommit(error))
                // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-return-setup-failed
            } // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-catch-setup
        }
    }

    /// Everything that can refuse a retry or an edit before the commit: the
    /// chat-model check, the mutation inputs, the quota preflight,
    /// `INPUT_TOO_LONG` and the image guards.
    async fn check_replacement(
        &self,
        scope: &AccessScope,
        chat: &Chat,
        user_id: Uuid,
        target: &MutationTarget,
        content: &str,
    ) -> Result<Checked, DomainError> {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-no-write-before-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-check-model
        let version = deps.policy.current_policy_version(user_id).await?;
        let snapshot = deps
            .policy
            .policy_snapshot(user_id, version.policy_version)
            .await?;
        if !snapshot
            .model_catalog
            .iter()
            .any(|entry| entry.id == chat.model)
        {
            return Err(DomainError::InvalidModel);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-check-model
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-load-mutation-inputs
        let loaded = load_mutation_inputs(
            &*deps.turns,
            &*deps.mutations,
            scope,
            chat.id,
            target.user_message.id,
            &deps.stream.settings().inputs,
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-load-mutation-inputs
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-preflight
        let decision = deps
            .quota
            .preflight(&PreflightRequest {
                tenant_id: chat.tenant_id,
                user_id,
                chat_id: chat.id,
                selected_model: chat.model.clone(),
                message_bytes: content.len() as u64,
                image_count: loaded.inputs.image_count,
                web_search_enabled: target.web_search_enabled,
                has_ready_file_search_documents: loaded.inputs.has_ready_file_search_documents,
                has_ready_code_interpreter_files: loaded.inputs.has_ready_code_interpreter_files,
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-preflight
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-input-too-long
        check_input_length(&decision.entry, content)?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-input-too-long
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-image-guards
        if loaded.inputs.image_count > 0 {
            if decision.snapshot.kill_switches.disable_images {
                return Err(DomainError::FeatureDisabled(DisabledFeature::Images));
            }
            if !decision.vision_input {
                return Err(DomainError::VisionNotSupported);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-mutation-image-guards
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-no-write-before-commit
        Ok(Checked {
            decision,
            inputs: loaded.inputs,
        })
    }

    /// Computes every value of the replacement transaction before it opens:
    /// the ids, the mutation time and the audit payload.
    fn replacement_plan(
        &self,
        op: MutationOp,
        chat: &Chat,
        user_id: Uuid,
        target: &MutationTarget,
        content: String,
    ) -> Result<ReplacementPlan, DomainError> {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-pregenerate-replacement
        let now = mutation_time(
            (deps.clock)(),
            target.turn.started_at,
            target.user_message.created_at,
        );
        // @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-request-ids:p1
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let new_user_message_id = Uuid::new_v4();
        let assistant_message_id = Uuid::new_v4();
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-pregenerate-replacement
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-precompute-replacement
        let audit = mutation_audit_event(&AuditInput {
            op,
            tenant_id: chat.tenant_id,
            actor_user_id: user_id,
            chat_id: chat.id,
            target_request_id: target.turn.request_id,
            new_request_id: Some(new_request_id),
            at: now,
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-precompute-replacement
        Ok(ReplacementPlan {
            op,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            user_id,
            target_turn_id: target.turn.id,
            target_request_id: target.turn.request_id,
            target_started_at: target.turn.started_at,
            target_user_message: MessageBoundary {
                created_at: target.user_message.created_at,
                id: target.user_message.id,
            },
            new_turn_id,
            new_request_id,
            new_user_message_id,
            assistant_message_id,
            content,
            web_search_enabled: target.web_search_enabled,
            now,
            audit,
        })
    }

    /// Runs the replacement transaction; a unique violation of the new turn's
    /// INSERT is a concurrent running turn.
    async fn commit_replacement(
        &self,
        scope: &AccessScope,
        plan: &ReplacementPlan,
    ) -> Result<(), DomainError> {
        match self.deps.mutations.replace_turn(plan).await {
            Err(DomainError::UniqueViolation) => Err(self.running_race(scope, plan.chat_id).await),
            other => other,
        }
    }

    /// The error of a unique violation: the violation names no constraint, and
    /// only a running turn of the chat makes it the one-running-turn index.
    // @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-concurrency:p1
    async fn running_race(&self, scope: &AccessScope, chat_id: Uuid) -> DomainError {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-running-race
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-generation-in-progress
        match self.deps.turns.find_running(scope, chat_id).await {
            Ok(Some(_)) => DomainError::GenerationInProgress,
            Ok(None) => DomainError::NotLatestTurn,
            Err(error) => error,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-return-generation-in-progress
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-replace-turn:p1:inst-if-running-race
    }

    async fn start_replacement(
        &self,
        setup: ReplacementSetup<'_>,
    ) -> Result<(StreamStarted, RelayHandle, oneshot::Sender<()>), DomainError> {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-load-history
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-history-after-commit
        let history = load_replacement_history(
            &*deps.turns,
            &*deps.mutations,
            setup.scope,
            setup.chat.id,
            setup.plan.new_request_id,
            deps.stream.settings().inputs.recent_messages_limit,
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-history-after-commit
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-run-replacement:p1:inst-load-history
        let mut inputs = setup.inputs;
        inputs.boundary = history.boundary;
        inputs.summary = history.summary;
        inputs.recent = history.recent;
        deps.stream
            .start_replacement(ReplacementStart {
                ctx: setup.ctx,
                chat: setup.chat,
                user_id: setup.user_id,
                request_id: setup.plan.new_request_id,
                turn_id: setup.plan.new_turn_id,
                assistant_message_id: setup.plan.assistant_message_id,
                user_message_at: setup.plan.now,
                decision: setup.decision,
                inputs,
                content: setup.content,
                accepted_at: setup.accepted_at,
            })
            .await
    }

    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    ///
    /// `AuthzDenied`, `PdpUnavailable`, `NotFound { Chat | Turn }`,
    /// `NotLatestTurn`, `TurnNotTerminal`, `Database` / `Internal`.
    // @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-delete:p1
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let accepted_at = Instant::now();
        let result = self.delete_inner(ctx, chat_id, request_id).await;
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-record-delete
        match &result {
            Ok(()) => {
                self.deps.metrics.turn_mutation_latency_ms(
                    MutationOp::Delete,
                    accepted_at.elapsed().as_secs_f64() * 1000.0,
                );
                self.deps
                    .metrics
                    .turn_mutation(MutationOp::Delete, MutationResult::Ok);
            }
            Err(error) => self
                .deps
                .metrics
                .turn_mutation(MutationOp::Delete, MutationResult::before_commit(error)),
        }
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-record-delete
        result
    }

    async fn delete_inner(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-authorize-delete
        let scope = deps
            .authz
            .chat_scope(ctx, ChatAction::DeleteTurn, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-authorize-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-load-chat-delete
        let chat = deps.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-load-chat-delete
        let user_id = ctx.subject_id();
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-preview-delete
        let target = preview(&*deps.mutations, &scope, chat.id, user_id, request_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-preview-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-precompute-delete
        let now = mutation_time(
            (deps.clock)(),
            target.turn.started_at,
            target.user_message.created_at,
        );
        let plan = DeletePlan {
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            user_id,
            target_turn_id: target.turn.id,
            target_request_id: target.turn.request_id,
            target_started_at: target.turn.started_at,
            target_user_message: MessageBoundary {
                created_at: target.user_message.created_at,
                id: target.user_message.id,
            },
            now,
            audit: mutation_audit_event(&AuditInput {
                op: MutationOp::Delete,
                tenant_id: chat.tenant_id,
                actor_user_id: user_id,
                chat_id: chat.id,
                target_request_id: target.turn.request_id,
                new_request_id: None,
                at: now,
            })?,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-remove-turn:p1:inst-precompute-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-run-delete
        deps.mutations.delete_turn(&plan).await
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-run-delete
    }
}

/// What the checks before the commit hand to the commit and the setup.
struct Checked {
    decision: PreflightDecision,
    inputs: TurnInputs,
}

/// What the post-commit setup needs.
struct ReplacementSetup<'a> {
    ctx: SecurityContext,
    scope: &'a AccessScope,
    chat: &'a Chat,
    user_id: Uuid,
    plan: &'a ReplacementPlan,
    decision: PreflightDecision,
    inputs: TurnInputs,
    content: &'a str,
    accepted_at: Instant,
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
