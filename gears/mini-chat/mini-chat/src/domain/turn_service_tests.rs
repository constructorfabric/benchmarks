#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Turn mutation tests on in-memory `SQLite` with a running outbox, the fake
//! model-policy plugin, the scripted fake LLM provider and the owner PDP
//! (shared with the stream service tests).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{KillSwitches, MiniChatModelPolicyPluginClientV1, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::secure::{DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::*;
use crate::config::MiniChatConfig;
use crate::domain::chat_service::tests::{OwnerPdp, ctx_of, owner_authz};
use crate::domain::error::{ExhaustedLimit, OutOfRangeReason, QuotaScope};
use crate::domain::llm::{LlmInputItem, LlmProvider};
use crate::domain::models_service::tests::catalog_entry;
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, BucketSettlement, FinalizedTurnRecord, MessageRole, NewChat,
    NewRunningTurn, QuotaBucketKey, QuotaOwner, QuotaRepository, QuotaUsageRecord, ToolCallCounts,
    TurnFinalization, TurnKey, TurnKeyRecord, TurnPreflightColumns, TurnRecord, TurnRepository,
};
use crate::domain::quota_service::{QuotaService, QuotaSettings};
use crate::domain::stream::service::FAIL_UNSTARTED_ATTEMPTS;
use crate::domain::stream::service::tests::{
    BIG, FakePolicy, FakeProvider, FakeResolver, drain, test_catalog, tier_limits,
};
use crate::domain::stream::{LiveStream, SendRequest, StreamEvent, StreamSettings};
use crate::infra::db::entities::{
    chat_turns, message_attachments, messages, quota_usage, thread_summaries,
};
use crate::infra::db::repo::DbRepository;
use crate::infra::db::repo::chats as chat_repo;
use crate::infra::db::repo::tests::{
    AttachmentSeed, Delivered, MessageSeed, TENANT_A, TENANT_B, TestStore, USER_A1, USER_A2,
    USER_B1, at, chat_row, message_rows, seed_attachment, seed_message,
};
use crate::infra::outbox::OutboxQueues;

// ── Quota repository with an over-limit reserve re-check ─────────────────

/// The `quota_usage` repository whose in-transaction read (the reserve
/// re-check) reports every bucket fully spent while `overspent` is set:
/// simulates a concurrent spend between the preflight and the reserve.
struct OverspentQuota {
    inner: Arc<DbRepository>,
    overspent: AtomicBool,
}

#[async_trait]
impl QuotaRepository for OverspentQuota {
    async fn read_buckets(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        for_update: bool,
    ) -> Result<Vec<QuotaUsageRecord>, DomainError> {
        self.inner
            .read_buckets(scope, owner, keys, for_update)
            .await
    }

    async fn read_buckets_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
    ) -> Result<Vec<QuotaUsageRecord>, DomainError> {
        let mut rows = self
            .inner
            .read_buckets_in_tx(tx, scope, owner, keys)
            .await?;
        if self.overspent.load(Ordering::SeqCst) {
            for row in &mut rows {
                row.spent_credits_micro = BIG;
            }
        }
        Ok(rows)
    }

    async fn add_reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        credits_micro: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        self.inner
            .add_reserve_in_tx(tx, scope, owner, keys, credits_micro, now)
            .await
    }

    async fn settle_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        owner: QuotaOwner,
        settlements: &[BucketSettlement],
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        self.inner
            .settle_in_tx(tx, scope, owner, settlements, now)
            .await
    }
}

// ── Turn repository with failing unstarted-turn CAS ──────────────────────

/// The `chat_turns` repository whose `fail_unstarted` fails while
/// `fail_unstarted_failures` is positive (one failure per call): simulates a
/// transient storage error after a retry / edit setup failure.
struct FlakyTurns {
    inner: Arc<DbRepository>,
    fail_unstarted_failures: AtomicU32,
    fail_unstarted_calls: AtomicU32,
}

#[async_trait]
impl TurnRepository for FlakyTurns {
    async fn find_active_by_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        self.inner
            .find_active_by_request(scope, chat_id, request_id)
            .await
    }

    async fn find_by_request_any(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnKeyRecord>, DomainError> {
        self.inner
            .find_by_request_any(scope, chat_id, request_id)
            .await
    }

    async fn running_exists(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        self.inner.running_exists(scope, chat_id).await
    }

    async fn insert_running_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewRunningTurn,
    ) -> Result<(), DomainError> {
        self.inner.insert_running_in_tx(tx, scope, turn).await
    }

    async fn finalize_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        finalization: TurnFinalization,
    ) -> Result<Option<FinalizedTurnRecord>, DomainError> {
        self.inner.finalize_in_tx(tx, scope, finalization).await
    }

    async fn record_progress(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        counts: ToolCallCounts,
        progress_at: Option<OffsetDateTime>,
    ) -> Result<bool, DomainError> {
        self.inner
            .record_progress(scope, turn_id, counts, progress_at)
            .await
    }

    async fn find_mutation_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError> {
        self.inner
            .find_mutation_target(scope, chat_id, request_id)
            .await
    }

    async fn find_mutation_target_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTargetRecord>, DomainError> {
        self.inner
            .find_mutation_target_in_tx(tx, scope, chat_id, request_id)
            .await
    }

    async fn latest_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnKey>, DomainError> {
        self.inner.latest_active(scope, chat_id).await
    }

    async fn newer_active_exists_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        key: TurnKey,
    ) -> Result<bool, DomainError> {
        self.inner
            .newer_active_exists_in_tx(tx, scope, chat_id, key)
            .await
    }

    async fn soft_delete_terminal_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        delete: TurnSoftDelete,
    ) -> Result<bool, DomainError> {
        self.inner
            .soft_delete_terminal_in_tx(tx, scope, delete)
            .await
    }

    async fn insert_mutation_turn_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn: NewMutationTurn,
    ) -> Result<(), DomainError> {
        self.inner.insert_mutation_turn_in_tx(tx, scope, turn).await
    }

    async fn record_preflight_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        turn_id: Uuid,
        columns: TurnPreflightColumns,
    ) -> Result<bool, DomainError> {
        self.inner
            .record_preflight_in_tx(tx, scope, turn_id, columns)
            .await
    }

    async fn fail_unstarted(
        &self,
        scope: &AccessScope,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.fail_unstarted_calls.fetch_add(1, Ordering::SeqCst);
        let failing = self
            .fail_unstarted_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(DomainError::internal("injected fail_unstarted failure"));
        }
        self.inner
            .fail_unstarted(scope, turn_id, error_code, at)
            .await
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────

struct Fixture {
    store: Arc<TestStore>,
    pdp: Arc<OwnerPdp>,
    policy: Arc<FakePolicy>,
    provider: Arc<FakeProvider>,
    quota_repo: Arc<OverspentQuota>,
    turns_repo: Arc<FlakyTurns>,
    stream: Arc<StreamService>,
    service: TurnService,
    queues: OutboxQueues,
}

/// A seeded turn.
#[derive(Debug, Clone, Copy)]
struct Seeded {
    request_id: Uuid,
    turn: Uuid,
    user_message: Uuid,
}

/// How a turn is seeded.
struct TurnSpec {
    state: TurnState,
    offset_ms: i64,
    requester: Uuid,
    content: &'static str,
    web_search_enabled: bool,
    /// `(input_tokens, output_tokens)` of the answer of a completed turn.
    usage: (i64, i64),
}

impl TurnSpec {
    const fn completed(offset_ms: i64, content: &'static str) -> Self {
        Self {
            state: TurnState::Completed,
            offset_ms,
            requester: USER_A1,
            content,
            web_search_enabled: false,
            usage: (0, 0),
        }
    }
}

impl Fixture {
    async fn new() -> Self {
        Self::with_catalog(test_catalog()).await
    }

    async fn with_catalog(catalog: Vec<ModelCatalogEntry>) -> Self {
        let store = Arc::new(TestStore::new().await);
        let policy = FakePolicy::new(catalog);
        let provider = Arc::new(FakeProvider::default());
        let pdp = OwnerPdp::new();
        let authz = owner_authz(&pdp);
        let config = MiniChatConfig::default();
        let quota_repo = Arc::new(OverspentQuota {
            inner: Arc::clone(&store.repo),
            overspent: AtomicBool::new(false),
        });
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&quota_repo) as Arc<dyn QuotaRepository>,
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            Arc::clone(&authz),
            QuotaSettings::from_config(&config),
        ));
        let turns_repo = Arc::new(FlakyTurns {
            inner: Arc::clone(&store.repo),
            fail_unstarted_failures: AtomicU32::new(0),
            fail_unstarted_calls: AtomicU32::new(0),
        });
        let mut repos = store.repo.repositories();
        repos.turns = Arc::clone(&turns_repo) as Arc<dyn TurnRepository>;
        let stream = Arc::new(StreamService::new(
            store.db.clone(),
            repos.clone(),
            quota,
            Arc::clone(&authz),
            Arc::new(FakeResolver),
            Arc::clone(&provider) as Arc<dyn LlmProvider>,
            StreamSettings::from_config(&config),
        ));
        let service = TurnService::new(store.db.clone(), repos, authz, Arc::clone(&stream));
        Self {
            store,
            pdp,
            policy,
            provider,
            quota_repo,
            turns_repo,
            stream,
            service,
            queues: OutboxQueues::default(),
        }
    }

    fn ctx() -> SecurityContext {
        ctx_of(TENANT_A, USER_A1)
    }

    async fn chat(&self, model: &str) -> Uuid {
        let id = Uuid::new_v4();
        chat_repo::insert(
            &self.store.conn(),
            self.store.ts(),
            &AccessScope::allow_all(),
            NewChat {
                id,
                tenant_id: TENANT_A,
                user_id: USER_A1,
                model: model.to_owned(),
                title: None,
                now: at(-60_000),
            },
        )
        .await
        .unwrap();
        id
    }

    /// Sends `content` through `messages:stream` and waits for the end of
    /// the stream; returns the request id.
    async fn send_done(&self, chat_id: Uuid, content: &str) -> Uuid {
        let request_id = Uuid::new_v4();
        let start = self
            .stream
            .send(
                Self::ctx(),
                chat_id,
                SendRequest {
                    content: content.to_owned(),
                    request_id: Some(request_id),
                    ..SendRequest::default()
                },
                SendOptions::default(),
            )
            .await
            .unwrap();
        let StreamStart::Live(live) = start else {
            panic!("expected a live turn");
        };
        assert_eq!(last_name(&drain(live).await), "done");
        request_id
    }

    /// A turn with its user message (and, when completed, an answer) at
    /// `offset_ms`.
    async fn seed(&self, chat_id: Uuid, spec: TurnSpec) -> Seeded {
        let request_id = Uuid::new_v4();
        let user_message_id = seed_message(
            &self.store,
            chat_id,
            TENANT_A,
            MessageSeed {
                request_id: Some(request_id),
                content: spec.content.to_owned(),
                ..MessageSeed::new(MessageRole::User, spec.offset_ms)
            },
        )
        .await;
        let assistant = if spec.state == TurnState::Completed {
            Some(
                seed_message(
                    &self.store,
                    chat_id,
                    TENANT_A,
                    MessageSeed {
                        request_id: Some(request_id),
                        content: format!("Answer to {}", spec.content),
                        input_tokens: spec.usage.0,
                        output_tokens: spec.usage.1,
                        ..MessageSeed::new(MessageRole::Assistant, spec.offset_ms + 1)
                    },
                )
                .await,
            )
        } else {
            None
        };
        let turn_id = Uuid::new_v4();
        let started = self.store.stamp(at(spec.offset_ms));
        let terminal = spec.state != TurnState::Running;
        let model = chat_turns::ActiveModel {
            id: Set(turn_id),
            tenant_id: Set(TENANT_A),
            chat_id: Set(chat_id),
            request_id: Set(request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(spec.requester)),
            state: Set(spec.state.as_str().to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(assistant),
            error_code: Set(None),
            reserve_tokens: Set(None),
            max_output_tokens_applied: Set(None),
            reserved_credits_micro: Set(None),
            policy_version_applied: Set(None),
            effective_model: Set(None),
            minimal_generation_floor_applied: Set(None),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(started),
            last_progress_at: Set(Some(started)),
            web_search_enabled: Set(spec.web_search_enabled),
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            completed_at: Set(terminal.then_some(started)),
            updated_at: Set(started),
        };
        secure_insert::<chat_turns::Entity>(model, &AccessScope::allow_all(), &self.store.conn())
            .await
            .unwrap();
        Seeded {
            request_id,
            turn: turn_id,
            user_message: user_message_id,
        }
    }

    async fn turns(&self, chat_id: Uuid) -> Vec<chat_turns::Model> {
        let mut rows = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap();
        rows.sort_by_key(|row| (row.started_at.at(), row.id));
        rows
    }

    async fn turn(&self, chat_id: Uuid, request_id: Uuid) -> chat_turns::Model {
        self.turns(chat_id)
            .await
            .into_iter()
            .find(|turn| turn.request_id == request_id)
            .expect("turn row")
    }

    async fn messages_of(&self, chat_id: Uuid, request_id: Uuid) -> Vec<messages::Model> {
        let mut rows: Vec<messages::Model> = message_rows(&self.store, chat_id)
            .await
            .into_iter()
            .filter(|m| m.request_id == Some(request_id))
            .collect();
        rows.sort_by_key(|row| (row.created_at.at(), row.id));
        rows
    }

    async fn links_of(&self, message_id: Uuid) -> Vec<Uuid> {
        message_attachments::Entity::find()
            .filter(message_attachments::Column::MessageId.eq(message_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
            .into_iter()
            .map(|link| link.attachment_id)
            .collect()
    }

    async fn summaries(&self, chat_id: Uuid) -> Vec<thread_summaries::Model> {
        thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    async fn quota_rows(&self) -> Vec<quota_usage::Model> {
        quota_usage::Entity::find()
            .filter(quota_usage::Column::UserId.eq(USER_A1))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    /// The chat's committed thread summary with frontier `frontier` (a
    /// message row); `covered` messages get `is_compressed`.
    async fn seed_summary(&self, chat_id: Uuid, frontier: &messages::Model, covered: &[Uuid]) {
        let model = thread_summaries::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(TENANT_A),
            chat_id: Set(chat_id),
            summary_text: Set("Seeded summary.".to_owned()),
            summarized_up_to_created_at: Set(frontier.created_at),
            summarized_up_to_message_id: Set(frontier.id),
            token_estimate: Set(5),
            created_at: Set(self.store.stamp(at(0))),
            updated_at: Set(self.store.stamp(at(0))),
        };
        secure_insert::<thread_summaries::Entity>(
            model,
            &AccessScope::allow_all(),
            &self.store.conn(),
        )
        .await
        .unwrap();
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(true))
            .filter(messages::Column::Id.is_in(covered.iter().copied()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&self.store.conn())
            .await
            .unwrap();
    }

    async fn retry(&self, chat_id: Uuid, request_id: Uuid) -> Result<StreamStart, DomainError> {
        self.service
            .retry(Self::ctx(), chat_id, request_id, SendOptions::default())
            .await
    }

    async fn edit(
        &self,
        chat_id: Uuid,
        request_id: Uuid,
        content: &str,
    ) -> Result<StreamStart, DomainError> {
        self.service
            .edit(
                Self::ctx(),
                chat_id,
                request_id,
                content.to_owned(),
                SendOptions::default(),
            )
            .await
    }

    async fn mutate(
        &self,
        op: &TurnMutation,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        match op {
            TurnMutation::Retry => self.retry(chat_id, request_id).await.map(|_| ()),
            TurnMutation::Edit { content } => {
                self.edit(chat_id, request_id, content).await.map(|_| ())
            }
            TurnMutation::Delete => self.service.delete(&Self::ctx(), chat_id, request_id).await,
        }
    }

    /// The commit step of `op` over a preview taken earlier (the stale
    /// preview of a concurrent change).
    async fn commit_previewed(
        &self,
        op: &TurnMutation,
        preview: MutationPreview,
    ) -> Result<(), DomainError> {
        let ctx = Self::ctx();
        match op {
            TurnMutation::Delete => self.service.commit_delete(&ctx, preview).await,
            TurnMutation::Retry | TurnMutation::Edit { .. } => {
                let content = match op {
                    TurnMutation::Edit { content } => content.clone(),
                    _ => "retried".to_owned(),
                };
                let replacement = Replacement {
                    kind: op.clone(),
                    request_id: Uuid::new_v4(),
                    turn_id: Uuid::new_v4(),
                    user_message_id: Uuid::new_v4(),
                    content,
                    started_at: crate::domain::ports::storage_now(),
                };
                self.service
                    .commit(&ctx, preview, replacement)
                    .await
                    .map(|_| ())
            }
        }
    }

    /// Soft-deletes the turn `turn_id` outside the mutation service.
    async fn soft_delete_turn_row(&self, turn_id: Uuid) {
        chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::DeletedAt,
                Expr::value(Some(self.store.stamp(at(-1)))),
            )
            .filter(chat_turns::Column::Id.eq(turn_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&self.store.conn())
            .await
            .unwrap();
    }

    /// Mutation audit payloads delivered so far.
    fn audit(&self, event_type: &str) -> Vec<Delivered> {
        self.store
            .delivered(&self.queues.audit)
            .into_iter()
            .filter(|d| d.payload["event_type"] == json!(event_type))
            .collect()
    }
}

fn live(start: StreamStart) -> LiveStream {
    match start {
        StreamStart::Live(live) => live,
        StreamStart::Replay(events) => panic!("expected a live turn, got {events:?}"),
    }
}

fn last_name(events: &[StreamEvent]) -> &'static str {
    events.last().map_or("none", StreamEvent::name)
}

const OPS: [fn() -> TurnMutation; 3] = [
    || TurnMutation::Retry,
    || TurnMutation::Edit {
        content: "edited".to_owned(),
    },
    || TurnMutation::Delete,
];

fn assert_aborted(err: &DomainError, expected: AbortReason) {
    match err {
        DomainError::Aborted { reason } => assert_eq!(*reason, expected, "{err:?}"),
        other => panic!("expected 409 {}, got {other:?}", expected.as_str()),
    }
}

fn assert_state_violation(err: &DomainError) {
    assert!(
        matches!(
            err,
            DomainError::FailedPrecondition {
                subject: PreconditionSubject::TurnState
            }
        ),
        "{err:?}"
    );
}

// ── Preview: errors and their order ──────────────────────────────────────

#[tokio::test]
async fn blank_edit_content_is_rejected_before_authorization() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-100, "Q")).await;

    let err = fx
        .edit(chat_id, turn.request_id, " \n\t ")
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::EmptyContent,
                field: "content"
            }
        ),
        "{err:?}"
    );
    assert_eq!(fx.pdp.calls(), 0, "EMPTY_CONTENT precedes the PDP call");
}

#[tokio::test]
async fn unknown_or_foreign_chat_and_unknown_turn_are_not_found() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-100, "Q")).await;
    let other_chat = fx.chat("standard").await;

    for make in OPS {
        let op = make();
        let err = fx
            .mutate(&op, Uuid::new_v4(), turn.request_id)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::NotFound {
                    resource: ResourceKind::Chat,
                    ..
                }
            ),
            "{err:?}"
        );
        for (chat, request_id) in [(chat_id, Uuid::new_v4()), (other_chat, turn.request_id)] {
            let err = fx.mutate(&op, chat, request_id).await.unwrap_err();
            assert!(
                matches!(
                    err,
                    DomainError::NotFound {
                        resource: ResourceKind::Turn,
                        ..
                    }
                ),
                "{err:?}"
            );
        }
        for ctx in [ctx_of(TENANT_A, USER_A2), ctx_of(TENANT_B, USER_B1)] {
            let err = fx
                .service
                .preview(&ctx, chat_id, turn.request_id, &op)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    DomainError::NotFound {
                        resource: ResourceKind::Chat,
                        ..
                    }
                ),
                "{err:?}"
            );
        }
    }
}

#[tokio::test]
async fn running_target_is_a_state_violation_even_when_not_latest() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let running = fx
        .seed(
            chat_id,
            TurnSpec {
                state: TurnState::Running,
                ..TurnSpec::completed(-5_000, "still running")
            },
        )
        .await;
    fx.seed(chat_id, TurnSpec::completed(-1_000, "newer")).await;

    for make in OPS {
        let err = fx
            .mutate(&make(), chat_id, running.request_id)
            .await
            .unwrap_err();
        assert_state_violation(&err);
    }
    assert!(
        fx.turns(chat_id)
            .await
            .iter()
            .all(|t| t.deleted_at.is_none())
    );
}

#[tokio::test]
async fn older_or_deleted_target_and_newer_running_turn_are_not_latest() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let older = fx.seed(chat_id, TurnSpec::completed(-5_000, "older")).await;
    let latest = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "latest"))
        .await;

    for make in OPS {
        let err = fx
            .mutate(&make(), chat_id, older.request_id)
            .await
            .unwrap_err();
        assert_aborted(&err, AbortReason::NotLatestTurn);
    }

    fx.service
        .delete(&Fixture::ctx(), chat_id, latest.request_id)
        .await
        .unwrap();
    for make in OPS {
        let err = fx
            .mutate(&make(), chat_id, latest.request_id)
            .await
            .unwrap_err();
        assert_aborted(&err, AbortReason::NotLatestTurn);
    }

    fx.seed(
        chat_id,
        TurnSpec {
            state: TurnState::Running,
            ..TurnSpec::completed(-10, "newer running")
        },
    )
    .await;
    for make in OPS {
        let err = fx
            .mutate(&make(), chat_id, older.request_id)
            .await
            .unwrap_err();
        assert_aborted(&err, AbortReason::NotLatestTurn);
    }
}

#[tokio::test]
async fn turn_of_another_requester_is_permission_denied_after_the_latest_check() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let foreign = fx
        .seed(
            chat_id,
            TurnSpec {
                requester: USER_A2,
                ..TurnSpec::completed(-5_000, "asked by A2")
            },
        )
        .await;
    for make in OPS {
        let err = fx
            .mutate(&make(), chat_id, foreign.request_id)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::PermissionDenied {
                    resource: ResourceKind::Turn
                }
            ),
            "{err:?}"
        );
    }

    fx.seed(chat_id, TurnSpec::completed(-1_000, "newer")).await;
    let err = fx.retry(chat_id, foreign.request_id).await.unwrap_err();
    assert_aborted(&err, AbortReason::NotLatestTurn);
    assert!(fx.provider.requests().is_empty());
}

// ── Retry / edit ─────────────────────────────────────────────────────────

#[tokio::test]
async fn retry_replaces_the_latest_turn_atomically_and_streams_a_new_turn() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let first = fx.send_done(chat_id, "First question").await;
    let second = fx.send_done(chat_id, "Second question").await;
    let chat_before = chat_row(&fx.store, chat_id).await;
    let calls_before = fx.pdp.calls();

    let live = live(fx.retry(chat_id, second).await.unwrap());
    let new_rid = live.first.request_id;
    let message_id = live.first.message_id;
    assert!(live.first.is_new_turn);
    assert_eq!(last_name(&drain(live).await), "done");

    assert_eq!(fx.pdp.calls(), calls_before + 1, "one PDP evaluation");
    assert_ne!(new_rid, second);
    assert_eq!(new_rid.get_version_num(), 4);
    let old = fx.turn(chat_id, second).await;
    assert!(old.deleted_at.is_some(), "{old:?}");
    assert_eq!(old.replaced_by_request_id, Some(new_rid));
    assert_eq!(old.state, "completed");
    assert!(
        fx.messages_of(chat_id, second)
            .await
            .iter()
            .all(|m| m.deleted_at.is_some())
    );
    assert!(fx.turn(chat_id, first).await.deleted_at.is_none());

    let new = fx.turn(chat_id, new_rid).await;
    assert_eq!(new.state, "completed");
    assert_eq!(new.assistant_message_id, Some(message_id));
    assert_eq!(new.requester_user_id, Some(USER_A1));
    assert!(
        new.reserve_tokens.is_some() && new.effective_model.is_some(),
        "{new:?}"
    );
    let new_messages = fx.messages_of(chat_id, new_rid).await;
    let roles: Vec<(&str, &str)> = new_messages
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(roles, [("user", "Second question"), ("assistant", "Fine.")]);
    assert!(chat_row(&fx.store, chat_id).await.updated_at.at() > chat_before.updated_at.at());

    let sent = fx.provider.requests().last().cloned().unwrap();
    let texts: Vec<String> = sent
        .input
        .iter()
        .map(|item| match item {
            LlmInputItem::User { text, .. } | LlmInputItem::Assistant { text } => text.clone(),
            other => panic!("unexpected input item {other:?}"),
        })
        .collect();
    assert_eq!(texts, ["First question", "Fine.", "Second question"]);

    let delivered = fx.store.wait_delivered(&fx.queues.audit, 4).await;
    let retries: Vec<&Delivered> = delivered
        .iter()
        .filter(|d| d.payload["event_type"] == json!("turn_retry"))
        .collect();
    assert_eq!(retries.len(), 1, "{delivered:?}");
    let event = &retries[0].payload;
    assert_eq!(event["original_request_id"], json!(second));
    assert_eq!(event["new_request_id"], json!(new_rid));
    assert_eq!(event["actor_user_id"], json!(USER_A1));
    assert_eq!(event["chat_id"], json!(chat_id));
}

#[tokio::test]
async fn edit_uses_the_new_content_and_keeps_the_web_search_flag() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let turn = fx
        .seed(
            chat_id,
            TurnSpec {
                web_search_enabled: true,
                ..TurnSpec::completed(-1_000, "Original")
            },
        )
        .await;

    let live = live(
        fx.edit(chat_id, turn.request_id, "Edited text")
            .await
            .unwrap(),
    );
    let new_rid = live.first.request_id;
    drain(live).await;

    let new = fx.turn(chat_id, new_rid).await;
    assert!(
        new.web_search_enabled,
        "retry/edit reuse web_search_enabled"
    );
    let user = &fx.messages_of(chat_id, new_rid).await[0];
    assert_eq!(
        (user.role.as_str(), user.content.as_str()),
        ("user", "Edited text")
    );
    let sent = fx.provider.requests().last().cloned().unwrap();
    assert!(
        sent.tools
            .iter()
            .any(|tool| matches!(tool, crate::domain::llm::LlmTool::WebSearch { .. })),
        "{:?}",
        sent.tools
    );
    fx.store.wait_delivered(&fx.queues.audit, 2).await;
    assert_eq!(fx.audit("turn_edit").len(), 1);
    assert!(fx.audit("turn_retry").is_empty());
}

#[tokio::test]
async fn retry_copies_live_attachment_links_and_resends_live_images() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let turn = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "Describe"))
        .await;
    let live_image = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "live.png"),
    )
    .await;
    let gone_image = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed {
            deleted: true,
            ..AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "gone.png")
        },
    )
    .await;
    for (id, offset) in [(live_image, -900), (gone_image, -800)] {
        crate::infra::db::repo::tests::link_attachment(
            &fx.store,
            chat_id,
            TENANT_A,
            turn.user_message,
            id,
            offset,
        )
        .await;
    }

    let live = live(fx.retry(chat_id, turn.request_id).await.unwrap());
    let new_rid = live.first.request_id;
    drain(live).await;

    let new_user = fx.messages_of(chat_id, new_rid).await[0].clone();
    assert_eq!(fx.links_of(new_user.id).await, [live_image]);
    let mut old_links = fx.links_of(turn.user_message).await;
    old_links.sort();
    let mut expected = vec![live_image, gone_image];
    expected.sort();
    assert_eq!(old_links, expected, "old links stay for audit");
    let sent = fx.provider.requests().last().cloned().unwrap();
    let Some(LlmInputItem::User {
        text,
        image_file_ids,
    }) = sent.input.last()
    else {
        panic!("{:?}", sent.input);
    };
    assert_eq!(text, "Describe");
    assert_eq!(
        image_file_ids,
        &[format!("file-secret-{}", live_image.simple())]
    );
}

// ── Preflight before the mutation ────────────────────────────────────────

#[tokio::test]
async fn preflight_rejections_change_nothing() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("text-only").await;
    let turn = fx
        .seed(
            chat_id,
            TurnSpec {
                web_search_enabled: true,
                ..TurnSpec::completed(-1_000, "Search and look")
            },
        )
        .await;
    let image = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "i.png"),
    )
    .await;
    crate::infra::db::repo::tests::link_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        turn.user_message,
        image,
        -900,
    )
    .await;
    let chat_before = chat_row(&fx.store, chat_id).await;
    let messages_before = message_rows(&fx.store, chat_id).await;

    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::VisionNotSupported,
                ..
            }
        ),
        "{err:?}"
    );

    fx.policy.publish(
        test_catalog(),
        KillSwitches {
            disable_web_search: true,
            ..KillSwitches::default()
        },
    );
    let err = fx
        .edit(chat_id, turn.request_id, "again")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::FailedPrecondition {
                subject: PreconditionSubject::WebSearch
            }
        ),
        "{err:?}"
    );

    fx.policy.publish(test_catalog(), KillSwitches::default());
    fx.policy
        .set_limits(tier_limits(0, BIG), tier_limits(0, BIG));
    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::ResourceExhausted {
                limit: ExhaustedLimit::Quota(QuotaScope::Tokens)
            }
        ),
        "{err:?}"
    );

    let turns = fx.turns(chat_id).await;
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert!(turns[0].deleted_at.is_none() && turns[0].replaced_by_request_id.is_none());
    assert_eq!(message_rows(&fx.store, chat_id).await, messages_before);
    assert_eq!(chat_row(&fx.store, chat_id).await, chat_before);
    assert!(fx.provider.requests().is_empty());
    assert!(fx.quota_rows().await.is_empty(), "no reserve");
}

// ── Setup failure after the commit ───────────────────────────────────────

async fn assert_unstarted_failure(fx: &Fixture, chat_id: Uuid, old: Uuid, code: &str) {
    let old_row = fx.turn(chat_id, old).await;
    assert!(
        old_row.deleted_at.is_some(),
        "the commit happened: {old_row:?}"
    );
    let new_rid = old_row.replaced_by_request_id.expect("replacement");
    let new = fx.turn(chat_id, new_rid).await;
    assert_eq!(new.state, "failed", "{new:?}");
    assert_eq!(new.error_code.as_deref(), Some(code));
    assert!(
        new.completed_at.is_some() && new.deleted_at.is_none(),
        "{new:?}"
    );
    assert!(new.assistant_message_id.is_none());
    assert!(new.reserve_tokens.is_none(), "{new:?}");
    assert!(new.max_output_tokens_applied.is_none());
    assert!(new.reserved_credits_micro.is_none());
    assert!(new.policy_version_applied.is_none());
    assert!(new.effective_model.is_none());
    assert!(new.minimal_generation_floor_applied.is_none());
    assert!(fx.provider.requests().is_empty(), "no provider call");
    assert!(
        fx.quota_rows()
            .await
            .iter()
            .all(|row| row.reserved_credits_micro == 0 && row.spent_credits_micro == 0),
        "no reserve and no settlement"
    );
}

#[tokio::test]
async fn context_budget_failure_after_the_commit_marks_the_new_turn_failed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("tiny").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-1_000, "hi")).await;

    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();

    assert!(
        matches!(
            err,
            DomainError::OutOfRange {
                reason: OutOfRangeReason::ContextBudgetExceeded,
                ..
            }
        ),
        "{err:?}"
    );
    assert_unstarted_failure(&fx, chat_id, turn.request_id, "context_length_exceeded").await;
    fx.store.wait_delivered(&fx.queues.audit, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(fx.audit("turn_retry").len(), 1);
    assert_eq!(
        fx.store.delivered(&fx.queues.audit).len(),
        1,
        "only turn_retry"
    );
    assert!(
        fx.store.delivered(&fx.queues.usage).is_empty(),
        "no usage event"
    );
}

#[tokio::test]
async fn reserve_recheck_failure_after_the_commit_is_quota_exceeded() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-1_000, "hi")).await;
    fx.quota_repo.overspent.store(true, Ordering::SeqCst);

    let err = fx
        .edit(chat_id, turn.request_id, "changed")
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            DomainError::ResourceExhausted {
                limit: ExhaustedLimit::Quota(QuotaScope::Tokens)
            }
        ),
        "{err:?}"
    );
    assert_unstarted_failure(&fx, chat_id, turn.request_id, "quota_exceeded").await;
}

#[tokio::test]
async fn provider_resolution_failure_after_the_commit_is_turn_setup_failed() {
    let mut catalog = test_catalog();
    catalog[0] = catalog_entry(
        "standard",
        json!({ "provider_id": "unknown-provider", "max_output_tokens": 1000 }),
    );
    let fx = Fixture::with_catalog(catalog).await;
    let chat_id = fx.chat("standard").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-1_000, "hi")).await;

    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    assert_unstarted_failure(&fx, chat_id, turn.request_id, "turn_setup_failed").await;

    // The chat is not blocked: the next message streams.
    fx.policy.publish(test_catalog(), KillSwitches::default());
    fx.send_done(chat_id, "next").await;
}

// ── Commit atomicity ─────────────────────────────────────────────────────

#[tokio::test]
async fn losing_the_running_index_race_is_generation_in_progress_and_rolls_back() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    // An older running turn the preview does not look at (only the target's
    // state and position are checked) occupies the running-turn index.
    fx.seed(
        chat_id,
        TurnSpec {
            state: TurnState::Running,
            ..TurnSpec::completed(-5_000, "racing")
        },
    )
    .await;
    let target = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "target"))
        .await;
    let messages_before = message_rows(&fx.store, chat_id).await;
    let chat_before = chat_row(&fx.store, chat_id).await;

    for op in [
        TurnMutation::Retry,
        TurnMutation::Edit {
            content: "x".to_owned(),
        },
    ] {
        let err = fx
            .mutate(&op, chat_id, target.request_id)
            .await
            .unwrap_err();
        assert_aborted(&err, AbortReason::GenerationInProgress);
    }

    let turns = fx.turns(chat_id).await;
    assert_eq!(turns.len(), 2, "{turns:?}");
    let row = fx.turn(chat_id, target.request_id).await;
    assert!(row.deleted_at.is_none() && row.replaced_by_request_id.is_none());
    assert_eq!(message_rows(&fx.store, chat_id).await, messages_before);
    assert_eq!(chat_row(&fx.store, chat_id).await, chat_before);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(fx.store.delivered(&fx.queues.audit).is_empty());
    assert!(fx.provider.requests().is_empty());
}

/// The rows a rejected stale commit must leave untouched.
struct Snapshot {
    turns: Vec<chat_turns::Model>,
    messages: Vec<messages::Model>,
}

impl Fixture {
    async fn snapshot(&self, chat_id: Uuid) -> Snapshot {
        Snapshot {
            turns: self.turns(chat_id).await,
            messages: message_rows(&self.store, chat_id).await,
        }
    }

    async fn assert_unchanged(&self, chat_id: Uuid, before: &Snapshot, op: &TurnMutation) {
        assert_eq!(self.turns(chat_id).await, before.turns, "{op:?}");
        assert_eq!(
            message_rows(&self.store, chat_id).await,
            before.messages,
            "{op:?}"
        );
    }

    /// No mutation audit event was delivered.
    async fn assert_no_audit(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(self.store.delivered(&self.queues.audit).is_empty());
    }
}

#[tokio::test]
async fn target_deleted_after_the_preview_is_not_latest_and_writes_nothing() {
    for make in OPS {
        let op = make();
        let fx = Fixture::new().await;
        let chat_id = fx.chat("standard").await;
        let target = fx
            .seed(chat_id, TurnSpec::completed(-1_000, "target"))
            .await;
        let preview = fx
            .service
            .preview(&Fixture::ctx(), chat_id, target.request_id, &op)
            .await
            .unwrap();
        fx.soft_delete_turn_row(target.turn).await;
        let before = fx.snapshot(chat_id).await;
        let chat_before = chat_row(&fx.store, chat_id).await;

        let err = fx.commit_previewed(&op, preview).await.unwrap_err();

        assert_aborted(&err, AbortReason::NotLatestTurn);
        fx.assert_unchanged(chat_id, &before, &op).await;
        assert_eq!(chat_row(&fx.store, chat_id).await, chat_before, "{op:?}");
        fx.assert_no_audit().await;
    }
}

#[tokio::test]
async fn replacement_turn_starts_at_the_preflight_instant() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let target = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "target"))
        .await;
    let ctx = Fixture::ctx();
    let preview = fx
        .service
        .preview(&ctx, chat_id, target.request_id, &TurnMutation::Retry)
        .await
        .unwrap();
    // The preflight instant (the reserve periods derive from it) precedes
    // the mutation commit.
    let preflight_at = crate::domain::ports::storage_now();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let replacement = Replacement {
        kind: TurnMutation::Retry,
        request_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        user_message_id: Uuid::new_v4(),
        content: "retried".to_owned(),
        started_at: preflight_at,
    };

    let turn = fx.service.commit(&ctx, preview, replacement).await.unwrap();

    let row = fx.turn(chat_id, turn.request_id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.started_at.at(), preflight_at);
    assert_eq!(
        row.last_progress_at
            .map(crate::infra::db::entities::DbTimestamp::at),
        Some(preflight_at)
    );
    let user_message = fx.messages_of(chat_id, turn.request_id).await;
    assert!(
        user_message[0].created_at.at() > preflight_at,
        "the commit writes later than the preflight instant"
    );
}

#[tokio::test]
async fn newer_turn_committed_after_the_preview_is_not_latest_and_writes_nothing() {
    for make in OPS {
        let op = make();
        let fx = Fixture::new().await;
        let chat_id = fx.chat("standard").await;
        let target = fx
            .seed(chat_id, TurnSpec::completed(-1_000, "target"))
            .await;
        let preview = fx
            .service
            .preview(&Fixture::ctx(), chat_id, target.request_id, &op)
            .await
            .unwrap();
        // A terminal turn committed through another path after the preview:
        // the guarded soft delete still matches the target, only the
        // in-transaction re-check sees the newer turn.
        fx.seed(chat_id, TurnSpec::completed(-500, "newer")).await;
        let before = fx.snapshot(chat_id).await;
        let chat_before = chat_row(&fx.store, chat_id).await;

        let err = fx.commit_previewed(&op, preview).await.unwrap_err();

        assert_aborted(&err, AbortReason::NotLatestTurn);
        fx.assert_unchanged(chat_id, &before, &op).await;
        assert_eq!(chat_row(&fx.store, chat_id).await, chat_before, "{op:?}");
        fx.assert_no_audit().await;
    }
}

#[tokio::test]
async fn chat_deleted_after_the_preflight_is_not_found_and_rolls_back() {
    for op in [
        TurnMutation::Retry,
        TurnMutation::Edit {
            content: "edited".to_owned(),
        },
    ] {
        let fx = Fixture::new().await;
        let chat_id = fx.chat("standard").await;
        let target = fx
            .seed(chat_id, TurnSpec::completed(-1_000, "target"))
            .await;
        let ctx = Fixture::ctx();
        let preview = fx
            .service
            .preview(&ctx, chat_id, target.request_id, &op)
            .await
            .unwrap();
        let content = match &op {
            TurnMutation::Edit { content } => content.clone(),
            _ => "target".to_owned(),
        };
        fx.stream
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
            .await
            .unwrap();
        assert!(
            chat_repo::mark_deleted(
                &fx.store.conn(),
                fx.store.ts(),
                &AccessScope::allow_all(),
                chat_id,
                at(-1),
            )
            .await
            .unwrap()
        );
        let before = fx.snapshot(chat_id).await;

        let err = fx.commit_previewed(&op, preview).await.unwrap_err();

        assert!(
            matches!(
                err,
                DomainError::NotFound {
                    resource: ResourceKind::Chat,
                    ..
                }
            ),
            "{op:?}: {err:?}"
        );
        fx.assert_unchanged(chat_id, &before, &op).await;
        assert_eq!(before.turns.len(), 1, "no new turn");
        assert!(before.turns[0].deleted_at.is_none(), "target kept");
        fx.assert_no_audit().await;
        assert!(fx.provider.requests().is_empty());
    }
}

// ── Preflight estimate of the replacement turn ───────────────────────────

#[tokio::test]
async fn retry_preflight_uses_the_usage_before_the_replaced_turn() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.seed(
        chat_id,
        TurnSpec {
            usage: (100, 50),
            ..TurnSpec::completed(-2_000, "first")
        },
    )
    .await;
    let second = fx
        .seed(
            chat_id,
            TurnSpec {
                usage: (1_000, 4_000),
                ..TurnSpec::completed(-1_000, "same text")
            },
        )
        .await;
    // Baseline: the same content without any prior usage.
    let empty_chat = fx.chat("standard").await;
    let baseline_rid = fx.send_done(empty_chat, "same text").await;
    let baseline = fx
        .turn(empty_chat, baseline_rid)
        .await
        .reserve_tokens
        .unwrap();

    let live = live(fx.retry(chat_id, second.request_id).await.unwrap());
    let new_rid = live.first.request_id;
    drain(live).await;

    let reserve = fx.turn(chat_id, new_rid).await.reserve_tokens.unwrap();
    assert_eq!(
        reserve - baseline,
        150,
        "prior_context_tokens is the usage of the turn before the replaced one"
    );
}

// ── Unstarted turn failure marking ───────────────────────────────────────

#[tokio::test]
async fn unstarted_turn_is_marked_failed_after_a_transient_storage_error() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("tiny").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-1_000, "hi")).await;
    fx.turns_repo
        .fail_unstarted_failures
        .store(1, Ordering::SeqCst);

    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();

    assert!(
        matches!(
            err,
            DomainError::OutOfRange {
                reason: OutOfRangeReason::ContextBudgetExceeded,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(fx.turns_repo.fail_unstarted_calls.load(Ordering::SeqCst), 2);
    assert_unstarted_failure(&fx, chat_id, turn.request_id, "context_length_exceeded").await;
}

#[tokio::test]
async fn unstarted_turn_failure_marking_is_bounded() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("tiny").await;
    let turn = fx.seed(chat_id, TurnSpec::completed(-1_000, "hi")).await;
    fx.turns_repo
        .fail_unstarted_failures
        .store(u32::MAX, Ordering::SeqCst);

    let err = fx.retry(chat_id, turn.request_id).await.unwrap_err();

    assert!(
        matches!(err, DomainError::OutOfRange { .. }),
        "the setup error is returned: {err:?}"
    );
    assert_eq!(
        fx.turns_repo.fail_unstarted_calls.load(Ordering::SeqCst),
        FAIL_UNSTARTED_ATTEMPTS
    );
}

// ── Delete ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_soft_deletes_without_replacement_or_preflight() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("text-only").await;
    let first = fx.seed(chat_id, TurnSpec::completed(-2_000, "first")).await;
    let second = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "second"))
        .await;
    fx.policy.set_limits(tier_limits(0, 0), tier_limits(0, 0));
    let first_before = fx.turn(chat_id, first.request_id).await;

    fx.service
        .delete(&Fixture::ctx(), chat_id, second.request_id)
        .await
        .unwrap();

    let row = fx.turn(chat_id, second.request_id).await;
    assert!(row.deleted_at.is_some() && row.replaced_by_request_id.is_none());
    assert_eq!(row.state, "completed");
    assert!(
        fx.messages_of(chat_id, second.request_id)
            .await
            .iter()
            .all(|m| m.deleted_at.is_some())
    );
    assert_eq!(fx.turn(chat_id, first.request_id).await, first_before);
    assert_eq!(fx.turns(chat_id).await.len(), 2, "delete creates no turn");
    assert!(fx.provider.requests().is_empty());
    let delivered = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    let event = &delivered[0].payload;
    assert_eq!(event["event_type"], json!("turn_delete"));
    assert_eq!(event["request_id"], json!(second.request_id));
    assert!(event.get("new_request_id").is_none(), "{event}");

    // The previous turn becomes the latest.
    let preview = fx
        .service
        .preview(
            &Fixture::ctx(),
            chat_id,
            first.request_id,
            &TurnMutation::Delete,
        )
        .await
        .unwrap();
    assert_eq!(preview.target.id, first.turn);
}

// ── Summary interaction ──────────────────────────────────────────────────

#[tokio::test]
async fn summary_at_or_after_the_mutated_user_message_is_dropped() {
    for make in OPS {
        let fx = Fixture::new().await;
        let chat_id = fx.chat("standard").await;
        let first = fx.seed(chat_id, TurnSpec::completed(-2_000, "first")).await;
        let second = fx
            .seed(chat_id, TurnSpec::completed(-1_000, "second"))
            .await;
        let first_messages = fx.messages_of(chat_id, first.request_id).await;
        let second_user = fx.messages_of(chat_id, second.request_id).await[0].clone();
        fx.seed_summary(
            chat_id,
            &second_user,
            &[first_messages[0].id, first_messages[1].id, second_user.id],
        )
        .await;

        let op = make();
        match &op {
            TurnMutation::Delete => fx.mutate(&op, chat_id, second.request_id).await.unwrap(),
            TurnMutation::Retry => {
                drain(live(fx.retry(chat_id, second.request_id).await.unwrap())).await;
            }
            TurnMutation::Edit { content } => {
                drain(live(
                    fx.edit(chat_id, second.request_id, content).await.unwrap(),
                ))
                .await;
            }
        }

        assert!(fx.summaries(chat_id).await.is_empty(), "{op:?}");
        assert!(
            message_rows(&fx.store, chat_id)
                .await
                .iter()
                .all(|m| !m.is_compressed),
            "{op:?}"
        );
    }
}

#[tokio::test]
async fn summary_before_the_mutated_turn_is_kept_and_applied() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let first = fx.seed(chat_id, TurnSpec::completed(-2_000, "first")).await;
    let second = fx
        .seed(chat_id, TurnSpec::completed(-1_000, "second"))
        .await;
    let first_messages = fx.messages_of(chat_id, first.request_id).await;
    fx.seed_summary(
        chat_id,
        &first_messages[1],
        &[first_messages[0].id, first_messages[1].id],
    )
    .await;
    let before = fx.summaries(chat_id).await;

    let live = live(fx.retry(chat_id, second.request_id).await.unwrap());
    assert_eq!(live.first.thread_summary_applied, Some(5));
    drain(live).await;

    assert_eq!(fx.summaries(chat_id).await, before);
    let sent = fx.provider.requests().last().cloned().unwrap();
    assert_eq!(sent.input.len(), 2, "{:?}", sent.input);
    let LlmInputItem::User { text, .. } = &sent.input[0] else {
        panic!("{:?}", sent.input);
    };
    assert!(text.ends_with("Seeded summary."), "{text}");
    let LlmInputItem::User { text, .. } = &sent.input[1] else {
        panic!("{:?}", sent.input);
    };
    assert_eq!(text, "second");
}

#[test]
fn frontier_cover_compares_created_at_then_id() {
    let t = at(0);
    let key = MessageKey {
        created_at: t,
        id: Uuid::from_u128(5),
    };
    let same = key;
    let later_id = MessageKey {
        created_at: t,
        id: Uuid::from_u128(6),
    };
    let earlier_id = MessageKey {
        created_at: t,
        id: Uuid::from_u128(4),
    };
    let later = MessageKey {
        created_at: t + time::Duration::milliseconds(1),
        id: Uuid::nil(),
    };
    assert!(covers(same, key));
    assert!(covers(later_id, key));
    assert!(covers(later, key));
    assert!(!covers(earlier_id, key));
}
