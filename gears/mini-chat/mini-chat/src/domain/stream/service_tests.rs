#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Stream service tests on in-memory `SQLite` with a running outbox, a fake
//! model-policy plugin, a scripted fake LLM provider and the owner PDP.
//! The fixtures are shared with the finalization tests.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits, UsageEvent, UserLimits,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::chat_service::tests::{OwnerPdp, ctx_of, owner_authz};
use crate::domain::error::{ExhaustedLimit, QuotaScope};
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmEvent, LlmEventStream, LlmFailure, LlmFailureKind,
    LlmUsage, ProviderTarget,
};
use crate::domain::models_service::tests::catalog_entry;
use crate::domain::ports::{
    BucketSettlement, NewChat, QuotaBucketKey, QuotaRepository, QuotaUsageRecord,
};
use crate::domain::quota::credits_micro_checked;
use crate::domain::quota_service::QuotaSettings;
use crate::domain::stream::events::{CitationSource, ToolPhase};
use crate::infra::db::entities::{
    chat_turns, message_attachments, messages, quota_usage, thread_summaries,
};
use crate::infra::db::repo::DbRepository;
use crate::infra::db::repo::chats as chat_repo;
use crate::infra::db::repo::tests::{
    AttachmentSeed, MessageSeed, TENANT_A, TestStore, TurnSeed, USER_A1, USER_A2, at, chat_row,
    message_rows, seed_attachment, seed_message, seed_turn, subject_scope,
};
use crate::infra::outbox::OutboxQueues;

// ── Fake model-policy plugin ─────────────────────────────────────────────

/// Policy plugin serving one replaceable snapshot per version and limits.
pub struct FakePolicy {
    version: AtomicU64,
    snapshots: Mutex<HashMap<u64, PolicySnapshot>>,
    limits: Mutex<(TierLimits, TierLimits)>,
    /// Every call fails with a transient error while set.
    failing: AtomicBool,
}

pub const BIG: i64 = 1_000_000_000_000;

pub const fn tier_limits(daily: i64, monthly: i64) -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: daily,
        limit_monthly_credits_micro: monthly,
    }
}

impl FakePolicy {
    pub fn new(catalog: Vec<ModelCatalogEntry>) -> Arc<Self> {
        let policy = Arc::new(Self {
            version: AtomicU64::new(0),
            snapshots: Mutex::new(HashMap::new()),
            limits: Mutex::new((tier_limits(BIG, BIG), tier_limits(BIG, BIG))),
            failing: AtomicBool::new(false),
        });
        policy.publish(catalog, KillSwitches::default());
        policy
    }

    pub fn publish(&self, catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches) {
        let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        self.snapshots.lock().unwrap().insert(
            version,
            PolicySnapshot {
                policy_version: version,
                model_catalog: catalog,
                kill_switches,
            },
        );
    }

    pub fn set_limits(&self, standard: TierLimits, premium: TierLimits) {
        *self.limits.lock().unwrap() = (standard, premium);
    }

    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), MiniChatModelPolicyPluginError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Transient(
                "policy source down".to_owned(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        self.check()?;
        Ok(self.version.load(Ordering::SeqCst))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        self.check()?;
        self.snapshots
            .lock()
            .unwrap()
            .get(&policy_version)
            .cloned()
            .ok_or(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        self.check()?;
        let (standard, premium) = *self.limits.lock().unwrap();
        Ok(UserLimits {
            user_id,
            policy_version,
            standard,
            premium,
        })
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

/// Catalog: `standard` (file search), `premium` (vision, all tools,
/// `web_search_context_size = medium`), `text-only` (no vision), `tiny`
/// (output cap above the context window).
pub fn test_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry(
            "standard",
            json!({
                "system_prompt": "You are helpful.",
                "max_output_tokens": 1000,
                "general_config": { "tool_support": { "file_search": true } },
            }),
        ),
        catalog_entry(
            "premium",
            json!({
                "tier": "premium",
                "system_prompt": "You are premium.",
                "multimodal_capabilities": ["VISION_INPUT"],
                "web_search_context_size": "medium",
                "max_tool_calls": 3,
                "general_config": {
                    "tool_support": { "web_search": true, "file_search": true, "code_interpreter": true },
                    "api_params": { "temperature": 0.5 },
                },
            }),
        ),
        catalog_entry("text-only", json!({ "system_prompt": "Text only." })),
        catalog_entry(
            "tiny",
            json!({ "context_window": 500, "max_output_tokens": 600 }),
        ),
    ]
}

// ── Fake LLM provider ────────────────────────────────────────────────────

/// One scripted provider call.
pub enum Script {
    /// Yields the events, then ends.
    Events(Vec<LlmEvent>),
    /// Yields the events, then hangs until cancelled.
    Hang(Vec<LlmEvent>),
    /// Fails before the stream opens.
    Fail(LlmFailure),
    /// Yields what the test sends, until the sender is dropped or the call
    /// is cancelled.
    Channel(mpsc::UnboundedReceiver<LlmEvent>),
}

/// Provider replaying scripts in order (default: a short completed answer)
/// and recording the requests.
#[derive(Default)]
pub struct FakeProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl FakeProvider {
    pub fn enqueue(&self, script: Script) {
        self.scripts.lock().unwrap().push_back(script);
    }

    /// A controllable call: the returned sender feeds its events.
    pub fn channel(&self) -> mpsc::UnboundedSender<LlmEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.enqueue(Script::Channel(rx));
        tx
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// A completed answer streamed as `chunks` with `usage`.
pub fn completed(chunks: &[&str], usage: Option<(i64, i64)>) -> Vec<LlmEvent> {
    let mut events: Vec<LlmEvent> = chunks
        .iter()
        .map(|chunk| LlmEvent::TextDelta((*chunk).to_owned()))
        .collect();
    events.push(completion(usage));
    events
}

pub fn completion(usage: Option<(i64, i64)>) -> LlmEvent {
    LlmEvent::Completed {
        response_id: Some("resp_internal123".to_owned()),
        usage: usage.map(|(input_tokens, output_tokens)| LlmUsage {
            input_tokens,
            output_tokens,
            ..LlmUsage::default()
        }),
        incomplete_reason: None,
        output_text: String::new(),
    }
}

#[async_trait]
impl LlmProvider for FakeProvider {
    async fn stream(
        &self,
        _target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        self.requests.lock().unwrap().push(req);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Script::Events(completed(&["Fine."], Some((10, 5)))));
        let stream: LlmEventStream = match script {
            Script::Events(events) => futures::stream::iter(events).boxed(),
            Script::Hang(events) => futures::stream::iter(events)
                .chain(futures::stream::pending())
                .boxed(),
            Script::Fail(failure) => return Err(failure),
            Script::Channel(rx) => futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed(),
        };
        Ok(stream.take_until(cancel.cancelled_owned()).boxed())
    }

    async fn complete(
        &self,
        _target: &ProviderTarget,
        _req: LlmRequest,
    ) -> Result<crate::domain::llm::LlmCompletion, LlmFailure> {
        Err(LlmFailure::provider_error("not scripted"))
    }
}

/// Resolver of the `openai` provider id; anything else fails.
pub struct FakeResolver;

impl LlmProviderResolver for FakeResolver {
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError> {
        if provider_id != "openai" {
            return Err(DomainError::internal("unknown provider"));
        }
        Ok(ProviderTarget {
            provider_id: provider_id.to_owned(),
            kind: ProviderKind::OpenaiResponses,
            alias: "api.openai.com".to_owned(),
            api_path: "/v1/responses".to_owned(),
            tenant_id,
            tenant_override: false,
        })
    }
}

// ── Quota repository with a preflight hook ───────────────────────────────

type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The `quota_usage` repository with a one-shot hook run at the first bucket
/// read (the quota preflight), after the idempotency and parallel checks:
/// simulates a concurrent request that wins the race.
pub struct HookedQuotaRepo {
    inner: Arc<DbRepository>,
    hook: Mutex<Option<Hook>>,
}

impl HookedQuotaRepo {
    pub fn set_hook(&self, hook: Hook) {
        *self.hook.lock().unwrap() = Some(hook);
    }
}

#[async_trait]
impl QuotaRepository for HookedQuotaRepo {
    async fn read_buckets(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        keys: &[QuotaBucketKey],
        for_update: bool,
    ) -> Result<Vec<QuotaUsageRecord>, DomainError> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook.await;
        }
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
        self.inner.read_buckets_in_tx(tx, scope, owner, keys).await
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

// ── Fixture ──────────────────────────────────────────────────────────────

pub struct Fixture {
    pub store: Arc<TestStore>,
    pub policy: Arc<FakePolicy>,
    pub provider: Arc<FakeProvider>,
    pub quota_repo: Arc<HookedQuotaRepo>,
    pub quota: Arc<QuotaService>,
    pub service: StreamService,
    pub queues: OutboxQueues,
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with_settings(|_| {}).await
    }

    pub async fn with_settings(adjust: impl FnOnce(&mut StreamSettings)) -> Self {
        let store = Arc::new(TestStore::new().await);
        let policy = FakePolicy::new(test_catalog());
        let provider = Arc::new(FakeProvider::default());
        let quota_repo = Arc::new(HookedQuotaRepo {
            inner: Arc::clone(&store.repo),
            hook: Mutex::new(None),
        });
        let authz = owner_authz(&OwnerPdp::new());
        let config = MiniChatConfig::default();
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&quota_repo) as Arc<dyn QuotaRepository>,
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            Arc::clone(&authz),
            QuotaSettings::from_config(&config),
        ));
        let mut settings = StreamSettings::from_config(&config);
        adjust(&mut settings);
        let service = StreamService::new(
            store.db.clone(),
            store.repo.repositories(),
            Arc::clone(&quota),
            authz,
            Arc::new(FakeResolver),
            Arc::clone(&provider) as Arc<dyn LlmProvider>,
            settings,
        );
        Self {
            store,
            policy,
            provider,
            quota_repo,
            quota,
            service,
            queues: OutboxQueues::default(),
        }
    }

    pub fn service_quota(&self) -> Arc<QuotaService> {
        Arc::clone(&self.quota)
    }

    /// A finalization context of the stored standard-model turn `turn`
    /// (preflight decision recomputed for a short message).
    pub async fn turn_context(&self, turn: &chat_turns::Model) -> TurnContext {
        let policy = self.quota.load_policy(USER_A1).await.unwrap();
        let owner = QuotaOwner {
            tenant_id: TENANT_A,
            user_id: USER_A1,
        };
        let decision = self
            .quota
            .preflight(PreflightInput {
                owner,
                selected_model: "standard",
                policy_version: policy.policy_version,
                snapshot: &policy.snapshot,
                user_limits: &policy.user_limits,
                request: PreflightRequest {
                    message_utf8_bytes: 4,
                    ..PreflightRequest::default()
                },
                prior_context_tokens: 0,
                now: storage_now(),
            })
            .await
            .unwrap();
        TurnContext {
            turn_id: turn.id,
            request_id: turn.request_id,
            chat_id: turn.chat_id,
            tenant_id: TENANT_A,
            user_id: USER_A1,
            scope: subject_scope(TENANT_A, USER_A1),
            assistant_message_id: Uuid::new_v4(),
            selected_model: "standard".to_owned(),
            decision,
            summary_due: false,
            trace_id: None,
        }
    }

    pub fn ctx() -> SecurityContext {
        ctx_of(TENANT_A, USER_A1)
    }

    pub async fn chat(&self, model: &str) -> Uuid {
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
                now: at(-1_000),
            },
        )
        .await
        .unwrap();
        id
    }

    pub async fn send(
        &self,
        chat_id: Uuid,
        request: SendRequest,
    ) -> Result<StreamStart, DomainError> {
        self.service
            .send(Self::ctx(), chat_id, request, SendOptions::default())
            .await
    }

    pub async fn live(&self, chat_id: Uuid, request: SendRequest) -> LiveStream {
        match self.send(chat_id, request).await {
            Ok(StreamStart::Live(live)) => live,
            other => panic!("expected a live stream, got {other:?}"),
        }
    }

    pub async fn turns(&self, chat_id: Uuid) -> Vec<chat_turns::Model> {
        chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    pub async fn turn(&self, chat_id: Uuid) -> chat_turns::Model {
        let mut turns = self.turns(chat_id).await;
        assert_eq!(turns.len(), 1, "{turns:?}");
        turns.remove(0)
    }

    pub async fn quota_rows(&self) -> Vec<quota_usage::Model> {
        quota_usage::Entity::find()
            .filter(quota_usage::Column::UserId.eq(USER_A1))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    pub async fn links(&self, chat_id: Uuid) -> Vec<message_attachments::Model> {
        message_attachments::Entity::find()
            .filter(message_attachments::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    /// Waits until the only turn of `chat_id` left `running`.
    pub async fn wait_terminal(&self, chat_id: Uuid) -> chat_turns::Model {
        for _ in 0..400 {
            let turn = self.turn(chat_id).await;
            if turn.state != "running" {
                return turn;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("turn of chat {chat_id} stayed running");
    }

    /// Sets the turn `turn_id` to `failed` as the orphan watchdog would.
    pub async fn finalize_externally(&self, turn_id: Uuid) {
        let now = self.store.stamp(at(0));
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value("failed"))
            .col_expr(chat_turns::Column::ErrorCode, Expr::value("orphan_timeout"))
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
            .filter(chat_turns::Column::Id.eq(turn_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&self.store.conn())
            .await
            .unwrap();
    }
}

/// The stream: `first`, then every event until the channel closes.
pub async fn drain(live: LiveStream) -> Vec<StreamEvent> {
    let LiveStream {
        first,
        mut rx,
        cancel: _cancel,
    } = live;
    let mut events = vec![StreamEvent::StreamStarted(first)];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return events,
            Err(elapsed) => panic!("stream did not end ({elapsed}): {events:?}"),
        }
    }
}

pub fn names(events: &[StreamEvent]) -> Vec<&'static str> {
    events.iter().map(StreamEvent::name).collect()
}

pub fn request(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        ..SendRequest::default()
    }
}

fn done_of(events: &[StreamEvent]) -> &Done {
    match events.last() {
        Some(StreamEvent::Done(done)) => done,
        other => panic!("expected done last, got {other:?}"),
    }
}

fn error_of(events: &[StreamEvent]) -> &crate::domain::stream::events::StreamError {
    match events.last() {
        Some(StreamEvent::Error(error)) => error,
        other => panic!("expected error last, got {other:?}"),
    }
}

/// Credits of the `standard` catalog model (1x input, 3x output).
pub fn standard_credits(input: i64, output: i64) -> i64 {
    credits_micro_checked(input, output, 1_000_000, 3_000_000).unwrap()
}

/// The estimated charge of a finalized standard-model turn row.
pub fn estimated_standard_charge(turn: &chat_turns::Model) -> i64 {
    let estimated_input =
        turn.reserve_tokens.unwrap() - i64::from(turn.max_output_tokens_applied.unwrap());
    standard_credits(
        estimated_input,
        i64::from(turn.minimal_generation_floor_applied.unwrap()),
    )
}

fn assert_settled(rows: &[quota_usage::Model], spent: i64) {
    let totals: Vec<&quota_usage::Model> = rows.iter().filter(|r| r.bucket == "total").collect();
    assert_eq!(totals.len(), 2, "{rows:?}");
    for row in totals {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.spent_credits_micro, spent, "{row:?}");
        assert_eq!(row.calls, 1, "{row:?}");
    }
}

fn assert_aborted(err: &DomainError, expected: AbortReason) {
    match err {
        DomainError::Aborted { reason } => assert_eq!(*reason, expected, "{err:?}"),
        other => panic!("expected 409 {}, got {other:?}", expected.as_str()),
    }
}

async fn insert_message_with_id(fx: &Fixture, chat_id: Uuid, id: Uuid) {
    let model = messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        request_id: Set(Some(Uuid::new_v4())),
        role: Set("assistant".to_owned()),
        content: Set("occupied".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(fx.store.stamp(at(-500))),
        deleted_at: Set(None),
    };
    secure_insert::<messages::Entity>(model, &AccessScope::allow_all(), &fx.store.conn())
        .await
        .unwrap();
}

// ── Happy path ───────────────────────────────────────────────────────────

#[tokio::test]
async fn completed_turn_streams_persists_settles_and_enqueues() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let before = chat_row(&fx.store, chat_id).await.updated_at.at();
    fx.provider.enqueue(Script::Events(completed(
        &["The capital ", "is Paris."],
        Some((211, 47)),
    )));
    let request_id = Uuid::new_v4();

    let live = fx
        .live(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("What is the capital of France?")
            },
        )
        .await;
    let first = live.first;
    let events = drain(live).await;

    assert_eq!(names(&events), ["stream_started", "delta", "delta", "done"]);
    assert_eq!(first.request_id, request_id);
    assert!(first.is_new_turn);
    assert_eq!(first.thread_summary_applied, None);
    let done = done_of(&events);
    assert_eq!(
        done.usage,
        DoneUsage {
            input_tokens: 211,
            output_tokens: 47
        }
    );
    assert_eq!(done.effective_model, "standard");
    assert_eq!(done.selected_model, "standard");
    assert_eq!(done.quota_decision, QuotaDecisionKind::Allow);
    assert_eq!(done.downgrade_from, None);
    assert_eq!(done.downgrade_reason, None);
    assert!(
        done.quota_warnings.as_ref().is_some_and(|w| !w.is_empty()),
        "{done:?}"
    );

    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.request_id, request_id);
    assert_eq!(turn.requester_type, "user");
    assert_eq!(turn.requester_user_id, Some(USER_A1));
    assert_eq!(turn.assistant_message_id, Some(first.message_id));
    assert_eq!(
        turn.provider_response_id.as_deref(),
        Some("resp_internal123")
    );
    assert_eq!(turn.error_code, None);
    assert!(turn.completed_at.is_some());
    assert_eq!(turn.effective_model.as_deref(), Some("standard"));
    assert_eq!(turn.max_output_tokens_applied, Some(1000));
    assert_eq!(turn.minimal_generation_floor_applied, Some(50));
    assert_eq!(turn.policy_version_applied, Some(1));
    assert!(turn.reserve_tokens.unwrap() > 1000);
    assert!(!turn.web_search_enabled);

    let rows = message_rows(&fx.store, chat_id).await;
    let user = rows.iter().find(|m| m.role == "user").unwrap();
    let assistant = rows.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(user.content, "What is the capital of France?");
    assert_eq!(user.request_id, Some(request_id));
    assert_eq!(user.content_type, "text");
    assert_eq!(user.request_kind, "chat");
    assert_eq!(user.features_used, json!([]));
    assert_eq!(assistant.id, first.message_id);
    assert_eq!(assistant.request_id, Some(request_id));
    assert_eq!(assistant.content, "The capital is Paris.");
    assert_eq!(assistant.model.as_deref(), Some("standard"));
    assert_eq!((assistant.input_tokens, assistant.output_tokens), (211, 47));
    assert_eq!(
        assistant.provider_response_id.as_deref(),
        Some("resp_internal123")
    );
    assert!(user.created_at.at() < assistant.created_at.at());
    assert!(chat_row(&fx.store, chat_id).await.updated_at.at() > before);

    assert_settled(&fx.quota_rows().await, standard_credits(211, 47));

    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = &usage[0].payload;
    assert_eq!(event["billing_outcome"], "completed");
    assert_eq!(event["settlement_method"], "actual");
    assert_eq!(event["terminal_state"], "completed");
    assert_eq!(event["actual_credits_micro"], standard_credits(211, 47));
    assert_eq!(event["turn_id"], json!(turn.id));
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].payload["event_type"], "turn_completed");
    assert_eq!(audit[0].payload["prompt"], "");
}

#[tokio::test]
async fn provider_request_carries_plan_identity_and_tools() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(completed(
        &["Apples are red."],
        Some((10, 4)),
    )));
    drain(fx.live(chat_id, request("What color are apples?")).await).await;

    drain(fx.live(chat_id, request("And bananas?")).await).await;

    let requests = fx.provider.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.model, "provider-standard");
    assert_eq!(second.instructions, "You are helpful.");
    assert_eq!(
        second.input,
        vec![
            LlmInputItem::User {
                text: "What color are apples?".to_owned(),
                image_file_ids: Vec::new()
            },
            LlmInputItem::Assistant {
                text: "Apples are red.".to_owned()
            },
            LlmInputItem::User {
                text: "And bananas?".to_owned(),
                image_file_ids: Vec::new()
            },
        ]
    );
    assert_eq!(second.max_output_tokens, 1000);
    assert!(second.tools.is_empty());
    assert_eq!(
        second.user,
        format!("{}{}", TENANT_A.simple(), USER_A1.simple())
    );
    assert_eq!(second.metadata.chat_id, chat_id.to_string());
    assert_eq!(second.metadata.tenant_id, TENANT_A.to_string());
    assert_eq!(second.metadata.user_id, USER_A1.to_string());
    assert_eq!(second.metadata.request_type, LlmRequestType::Chat);
    assert_eq!(second.metadata.feature, "none");
}

#[tokio::test]
async fn web_search_tool_and_guard_are_sent_on_a_supporting_model() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;

    let events = drain(
        fx.live(
            chat_id,
            SendRequest {
                web_search_enabled: true,
                ..request("News today?")
            },
        )
        .await,
    )
    .await;

    assert_eq!(names(&events).last(), Some(&"done"));
    let sent = &fx.provider.requests()[0];
    assert_eq!(
        sent.tools,
        vec![LlmTool::WebSearch {
            search_context_size: "medium".to_owned()
        }]
    );
    assert!(sent.instructions.starts_with("You are premium."));
    assert!(
        sent.instructions.contains("web_search"),
        "{}",
        sent.instructions
    );
    assert_eq!(sent.metadata.feature, "web_search");
    assert_eq!(sent.max_tool_calls, 3);
    assert_eq!(sent.api_params.temperature, Some(0.5));
    assert!(fx.turn(chat_id).await.web_search_enabled);
}

// ── Replay and conflicts ─────────────────────────────────────────────────

#[tokio::test]
async fn replay_of_completed_turn_is_side_effect_free() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(completed(
        &["Replay ", "me."],
        Some((321, 54)),
    )));
    let request_id = Uuid::new_v4();
    let original = fx
        .live(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("Original")
            },
        )
        .await;
    let message_id = original.first.message_id;
    drain(original).await;
    fx.store.wait_delivered(&fx.queues.usage, 1).await;
    let quota = fx.quota_rows().await;
    let turns = fx.turns(chat_id).await;
    let messages = message_rows(&fx.store, chat_id).await;

    let replay = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("Different wording")
            },
        )
        .await
        .unwrap();

    let StreamStart::Replay(events) = replay else {
        panic!("expected a replay");
    };
    assert_eq!(names(&events), ["stream_started", "delta", "done"]);
    let StreamEvent::StreamStarted(started) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(started.request_id, request_id);
    assert_eq!(started.message_id, message_id);
    assert!(!started.is_new_turn);
    assert_eq!(events[1], StreamEvent::text("Replay me."));
    let done = done_of(&events);
    assert_eq!(
        done.usage,
        DoneUsage {
            input_tokens: 321,
            output_tokens: 54
        }
    );
    assert_eq!(done.quota_decision, QuotaDecisionKind::Allow);
    assert_eq!(done.downgrade_from, None);
    assert_eq!(done.downgrade_reason, None);
    assert_eq!(done.quota_warnings, None);

    assert_eq!(
        fx.provider.requests().len(),
        1,
        "replay must not call the provider"
    );
    assert_eq!(fx.quota_rows().await, quota);
    assert_eq!(fx.turns(chat_id).await, turns);
    assert_eq!(message_rows(&fx.store, chat_id).await, messages);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fx.store.delivered(&fx.queues.usage).len(), 1);
    assert_eq!(fx.store.delivered(&fx.queues.audit).len(), 1);
}

#[tokio::test]
async fn replay_rebuilds_downgrade_from_the_stored_models() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let request_id = Uuid::new_v4();
    let assistant = seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed {
            request_id: Some(request_id),
            content: "Cheaper answer".to_owned(),
            model: Some("standard".to_owned()),
            input_tokens: 7,
            output_tokens: 3,
            ..MessageSeed::new(MessageRole::Assistant, -100)
        },
    )
    .await;
    seed_turn(
        &fx.store,
        chat_id,
        TENANT_A,
        TurnSeed {
            request_id,
            assistant_message_id: Some(assistant),
            ..TurnSeed::new(TurnState::Completed)
        },
    )
    .await;

    let Ok(StreamStart::Replay(events)) = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("again")
            },
        )
        .await
    else {
        panic!("expected a replay");
    };

    let done = done_of(&events);
    assert_eq!(done.effective_model, "standard");
    assert_eq!(done.selected_model, "premium");
    assert_eq!(done.quota_decision, QuotaDecisionKind::Downgrade);
    assert_eq!(done.downgrade_from.as_deref(), Some("premium"));
    assert_eq!(done.downgrade_reason, None);
    assert_eq!(
        done.usage,
        DoneUsage {
            input_tokens: 7,
            output_tokens: 3
        }
    );
}

#[tokio::test]
async fn reused_request_id_of_a_non_replayable_turn_is_a_conflict() {
    let fx = Fixture::new().await;
    for seed in [
        TurnSeed::new(TurnState::Running),
        TurnSeed::new(TurnState::Failed),
        TurnSeed::new(TurnState::Cancelled),
        TurnSeed {
            deleted: true,
            ..TurnSeed::new(TurnState::Completed)
        },
    ] {
        let chat_id = fx.chat("standard").await;
        let request_id = seed.request_id;
        seed_turn(&fx.store, chat_id, TENANT_A, seed).await;

        let err = fx
            .send(
                chat_id,
                SendRequest {
                    request_id: Some(request_id),
                    ..request("again")
                },
            )
            .await
            .unwrap_err();

        assert_aborted(&err, AbortReason::RequestIdConflict);
        assert!(message_rows(&fx.store, chat_id).await.is_empty());
        assert_eq!(fx.turns(chat_id).await.len(), 1);
    }
    assert!(fx.provider.requests().is_empty());
}

#[tokio::test]
async fn parallel_guard_rejects_and_replay_wins_over_it() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let completed_request = Uuid::new_v4();
    let assistant = seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed {
            request_id: Some(completed_request),
            ..MessageSeed::new(MessageRole::Assistant, -100)
        },
    )
    .await;
    seed_turn(
        &fx.store,
        chat_id,
        TENANT_A,
        TurnSeed {
            request_id: completed_request,
            assistant_message_id: Some(assistant),
            ..TurnSeed::new(TurnState::Completed)
        },
    )
    .await;
    seed_turn(
        &fx.store,
        chat_id,
        TENANT_A,
        TurnSeed::new(TurnState::Running),
    )
    .await;

    let with_new_id = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(Uuid::new_v4()),
                ..request("me too")
            },
        )
        .await
        .unwrap_err();
    let without_id = fx.send(chat_id, request("me three")).await.unwrap_err();
    let replay = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(completed_request),
                ..request("replay")
            },
        )
        .await
        .unwrap();

    assert_aborted(&with_new_id, AbortReason::TurnAlreadyRunning);
    assert_aborted(&without_id, AbortReason::TurnAlreadyRunning);
    assert!(matches!(replay, StreamStart::Replay(_)));
    assert_eq!(message_rows(&fx.store, chat_id).await.len(), 1);
    assert!(fx.quota_rows().await.is_empty());
}

/// The test catalog without the `standard` model.
fn catalog_without_standard() -> Vec<ModelCatalogEntry> {
    test_catalog()
        .into_iter()
        .filter(|entry| entry.id != "standard")
        .collect()
}

#[tokio::test]
async fn replay_depends_on_neither_the_catalog_nor_the_policy_plugin() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider
        .enqueue(Script::Events(completed(&["Kept."], Some((12, 4)))));
    let request_id = Uuid::new_v4();
    let replay_request = || SendRequest {
        request_id: Some(request_id),
        ..request("again")
    };
    drain(fx.live(chat_id, replay_request()).await).await;
    fx.store.wait_delivered(&fx.queues.usage, 1).await;

    fx.policy
        .publish(catalog_without_standard(), KillSwitches::default());
    let without_model = fx.send(chat_id, replay_request()).await.unwrap();
    fx.policy.set_failing(true);
    let plugin_down = fx.send(chat_id, replay_request()).await.unwrap();

    for start in [without_model, plugin_down] {
        let StreamStart::Replay(events) = start else {
            panic!("expected a replay");
        };
        assert_eq!(names(&events), ["stream_started", "delta", "done"]);
        assert_eq!(events[1], StreamEvent::text("Kept."));
        let done = done_of(&events);
        assert_eq!(done.effective_model, "standard");
        assert_eq!(done.selected_model, "standard");
        assert_eq!(
            done.usage,
            DoneUsage {
                input_tokens: 12,
                output_tokens: 4
            }
        );
    }
    assert_eq!(fx.provider.requests().len(), 1);
}

#[tokio::test]
async fn key_and_running_turn_conflicts_precede_model_and_plugin_checks() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let running = TurnSeed::new(TurnState::Running);
    let running_request = running.request_id;
    seed_turn(&fx.store, chat_id, TENANT_A, running).await;
    fx.policy
        .publish(catalog_without_standard(), KillSwitches::default());

    for plugin_down in [false, true] {
        fx.policy.set_failing(plugin_down);
        let reused = fx
            .send(
                chat_id,
                SendRequest {
                    request_id: Some(running_request),
                    ..request("same key")
                },
            )
            .await
            .unwrap_err();
        let new_key = fx
            .send(
                chat_id,
                SendRequest {
                    request_id: Some(Uuid::new_v4()),
                    ..request("new key")
                },
            )
            .await
            .unwrap_err();

        assert_aborted(&reused, AbortReason::RequestIdConflict);
        assert_aborted(&new_key, AbortReason::TurnAlreadyRunning);
    }
    fx.policy.set_failing(false);
    assert_eq!(fx.turns(chat_id).await.len(), 1);
    assert!(message_rows(&fx.store, chat_id).await.is_empty());
    assert!(fx.provider.requests().is_empty());

    // Without a conflict, the removed model is reported.
    let idle = fx.chat("standard").await;
    let err = fx.send(idle, request("hi")).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::InvalidModel,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn losing_the_running_index_race_is_turn_already_running() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let store = Arc::clone(&fx.store);
    fx.quota_repo.set_hook(Box::pin(async move {
        seed_turn(&store, chat_id, TENANT_A, TurnSeed::new(TurnState::Running)).await;
    }));
    let request_id = Uuid::new_v4();

    let err = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("racer")
            },
        )
        .await
        .unwrap_err();

    assert_aborted(&err, AbortReason::TurnAlreadyRunning);
    assert!(
        message_rows(&fx.store, chat_id).await.is_empty(),
        "the user message rolled back"
    );
    assert!(
        fx.quota_rows()
            .await
            .iter()
            .all(|row| row.reserved_credits_micro == 0),
        "the reserve rolled back"
    );
    assert!(fx.provider.requests().is_empty());
}

#[tokio::test]
async fn losing_the_same_key_race_is_request_id_conflict() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let request_id = Uuid::new_v4();
    let store = Arc::clone(&fx.store);
    fx.quota_repo.set_hook(Box::pin(async move {
        seed_message(
            &store,
            chat_id,
            TENANT_A,
            MessageSeed {
                request_id: Some(request_id),
                ..MessageSeed::new(MessageRole::User, -50)
            },
        )
        .await;
        seed_turn(
            &store,
            chat_id,
            TENANT_A,
            TurnSeed {
                request_id,
                ..TurnSeed::new(TurnState::Running)
            },
        )
        .await;
    }));

    let err = fx
        .send(
            chat_id,
            SendRequest {
                request_id: Some(request_id),
                ..request("same key")
            },
        )
        .await
        .unwrap_err();

    assert_aborted(&err, AbortReason::RequestIdConflict);
    assert_eq!(message_rows(&fx.store, chat_id).await.len(), 1);
    assert!(fx.provider.requests().is_empty());
}

#[tokio::test]
async fn chat_deleted_before_the_reserve_is_not_found_and_rolls_back() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let store = Arc::clone(&fx.store);
    fx.quota_repo.set_hook(Box::pin(async move {
        assert!(
            chat_repo::mark_deleted(
                &store.conn(),
                store.ts(),
                &AccessScope::allow_all(),
                chat_id,
                at(-1),
            )
            .await
            .unwrap()
        );
    }));

    let err = fx
        .send(chat_id, request("into a deleted chat"))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            DomainError::NotFound {
                resource: crate::domain::error::ResourceKind::Chat,
                ..
            }
        ),
        "{err:?}"
    );
    assert_nothing_left_behind(&fx, chat_id).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.store.delivered(&fx.queues.audit).is_empty());
}

// ── Pre-stream rejections ────────────────────────────────────────────────

async fn assert_nothing_left_behind(fx: &Fixture, chat_id: Uuid) {
    assert!(fx.turns(chat_id).await.is_empty());
    assert!(message_rows(&fx.store, chat_id).await.is_empty());
    assert!(fx.links(chat_id).await.is_empty());
    assert!(
        fx.quota_rows()
            .await
            .iter()
            .all(|row| row.reserved_credits_micro == 0 && row.spent_credits_micro == 0)
    );
    assert!(fx.provider.requests().is_empty());
}

#[tokio::test]
async fn request_validation_failures_leave_nothing_behind() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let doc = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "a.pdf"),
    )
    .await;
    let too_many: Vec<Uuid> = (0..55).map(|_| Uuid::new_v4()).collect();

    let cases = [
        (request("   \n"), InvalidArgumentReason::EmptyContent),
        (
            SendRequest {
                attachment_ids: vec![doc, doc],
                ..request("dup")
            },
            InvalidArgumentReason::InvalidAttachment,
        ),
        (
            SendRequest {
                attachment_ids: too_many,
                ..request("many")
            },
            InvalidArgumentReason::InvalidAttachment,
        ),
    ];
    for (req, reason) in cases {
        let err = fx.send(chat_id, req).await.unwrap_err();
        assert!(
            matches!(err, DomainError::InvalidArgument { reason: r, .. } if r == reason),
            "{err:?}"
        );
    }
    assert_nothing_left_behind(&fx, chat_id).await;
}

#[tokio::test]
async fn invalid_attachment_inside_the_reserve_transaction_rolls_everything_back() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let other_chat = fx.chat("standard").await;
    let good = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(
            AttachmentKind::Document,
            AttachmentStatus::Ready,
            "good.pdf",
        ),
    )
    .await;
    let pending = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Pending, "p.pdf"),
    )
    .await;
    let deleted = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed {
            deleted: true,
            ..AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "d.pdf")
        },
    )
    .await;
    let foreign = seed_attachment(
        &fx.store,
        other_chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "f.pdf"),
    )
    .await;

    for bad in [pending, deleted, foreign, Uuid::new_v4()] {
        let err = fx
            .send(
                chat_id,
                SendRequest {
                    attachment_ids: vec![good, bad],
                    ..request("see attached")
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::InvalidArgument {
                    reason: InvalidArgumentReason::InvalidAttachment,
                    ..
                }
            ),
            "{err:?}"
        );
    }
    assert_nothing_left_behind(&fx, chat_id).await;
}

#[tokio::test]
async fn valid_attachments_are_linked_to_the_user_message() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let doc = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "a.pdf"),
    )
    .await;
    let image = seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "i.png"),
    )
    .await;

    drain(
        fx.live(
            chat_id,
            SendRequest {
                attachment_ids: vec![doc, image],
                ..request("look")
            },
        )
        .await,
    )
    .await;

    let user = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.role == "user")
        .unwrap();
    let mut linked: Vec<Uuid> = fx
        .links(chat_id)
        .await
        .into_iter()
        .inspect(|link| assert_eq!(link.message_id, user.id))
        .map(|link| link.attachment_id)
        .collect();
    linked.sort();
    let mut expected = vec![doc, image];
    expected.sort();
    assert_eq!(linked, expected);
    let sent = &fx.provider.requests()[0];
    assert_eq!(
        sent.input.last(),
        Some(&LlmInputItem::User {
            text: "look".to_owned(),
            image_file_ids: vec![format!("file-secret-{}", image.simple())],
        })
    );
}

#[tokio::test]
async fn image_guards_reject_before_the_reserve() {
    let fx = Fixture::new().await;
    let text_chat = fx.chat("text-only").await;
    let image = seed_attachment(
        &fx.store,
        text_chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "i.png"),
    )
    .await;
    let err = fx
        .send(
            text_chat,
            SendRequest {
                attachment_ids: vec![image],
                ..request("what is this")
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::VisionNotSupported,
                field: "content_type"
            }
        ),
        "{err:?}"
    );
    assert_nothing_left_behind(&fx, text_chat).await;

    let premium_chat = fx.chat("premium").await;
    let mut images = Vec::new();
    for i in 0..5 {
        images.push(
            seed_attachment(
                &fx.store,
                premium_chat,
                TENANT_A,
                AttachmentSeed::new(
                    AttachmentKind::Image,
                    AttachmentStatus::Ready,
                    &format!("{i}.png"),
                ),
            )
            .await,
        );
    }
    let err = fx
        .send(
            premium_chat,
            SendRequest {
                attachment_ids: images.clone(),
                ..request("five")
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::OutOfRange {
                reason: OutOfRangeReason::TooManyImages,
                ..
            }
        ),
        "{err:?}"
    );

    fx.policy.publish(
        test_catalog(),
        KillSwitches {
            disable_images: true,
            ..KillSwitches::default()
        },
    );
    let err = fx
        .send(
            premium_chat,
            SendRequest {
                attachment_ids: images[..1].to_vec(),
                ..request("one")
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::FailedPrecondition {
                subject: PreconditionSubject::Images
            }
        ),
        "{err:?}"
    );
    assert_nothing_left_behind(&fx, premium_chat).await;
}

#[tokio::test]
async fn model_quota_and_budget_rejections_leave_nothing_behind() {
    let fx = Fixture::new().await;

    let removed = fx.chat("removed-model").await;
    let err = fx.send(removed, request("hi")).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::InvalidModel,
                ..
            }
        ),
        "{err:?}"
    );
    assert_nothing_left_behind(&fx, removed).await;

    let tiny = fx.chat("tiny").await;
    let err = fx.send(tiny, request("hi")).await.unwrap_err();
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
    assert_nothing_left_behind(&fx, tiny).await;

    let chat_id = fx.chat("standard").await;
    fx.policy
        .set_limits(tier_limits(0, BIG), tier_limits(0, BIG));
    let err = fx.send(chat_id, request("hi")).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::ResourceExhausted {
                limit: ExhaustedLimit::Quota(QuotaScope::Tokens)
            }
        ),
        "{err:?}"
    );
    assert_nothing_left_behind(&fx, chat_id).await;

    fx.policy
        .set_limits(tier_limits(BIG, BIG), tier_limits(BIG, BIG));
    fx.policy.publish(
        test_catalog(),
        KillSwitches {
            disable_web_search: true,
            ..KillSwitches::default()
        },
    );
    let err = fx
        .send(
            chat_id,
            SendRequest {
                web_search_enabled: true,
                ..request("search")
            },
        )
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
    assert_nothing_left_behind(&fx, chat_id).await;
}

#[tokio::test]
async fn foreign_or_unknown_chat_is_not_found() {
    let fx = Fixture::new().await;
    let foreign = Uuid::new_v4();
    chat_repo::insert(
        &fx.store.conn(),
        fx.store.ts(),
        &AccessScope::allow_all(),
        NewChat {
            id: foreign,
            tenant_id: TENANT_A,
            user_id: USER_A2,
            model: "standard".to_owned(),
            title: None,
            now: at(0),
        },
    )
    .await
    .unwrap();

    for chat_id in [foreign, Uuid::new_v4()] {
        let err = fx.send(chat_id, request("hello")).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
    }
    assert!(fx.turns(foreign).await.is_empty());
}

// ── Provider task outcomes ───────────────────────────────────────────────

#[tokio::test]
async fn client_disconnect_cancels_persists_partial_and_settles_estimated() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Hang(vec![
        LlmEvent::TextDelta("partial ".to_owned()),
        LlmEvent::TextDelta("answer".to_owned()),
    ]));
    let mut live = fx.live(chat_id, request("tell me a story")).await;
    let message_id = live.first.message_id;
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(5), live.rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name(), "delta");
    }

    live.cancel.cancel();
    drop(live);
    let turn = fx.wait_terminal(chat_id).await;

    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.error_code, None);
    assert_eq!(turn.assistant_message_id, Some(message_id));
    let assistant = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(assistant.id, message_id);
    assert_eq!(assistant.content, "partial answer");
    assert_settled(&fx.quota_rows().await, estimated_standard_charge(&turn));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["billing_outcome"], "aborted");
    assert_eq!(usage[0].payload["settlement_method"], "estimated");
    assert_eq!(usage[0].payload["usage"], serde_json::Value::Null);
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(audit[0].payload["event_type"], "turn_failed");
}

#[tokio::test]
async fn dropped_receiver_before_any_delta_cancels_without_a_message() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Hang(Vec::new()));
    let live = fx.live(chat_id, request("think silently")).await;

    drop(live.rx);
    let turn = fx.wait_terminal(chat_id).await;

    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    let roles: Vec<String> = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(roles, ["user"]);
    assert_settled(&fx.quota_rows().await, estimated_standard_charge(&turn));
}

#[tokio::test]
async fn provider_failure_with_usage_settles_actual() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(vec![
        LlmEvent::TextDelta("half".to_owned()),
        LlmEvent::Failed(LlmFailure {
            usage: Some(LlmUsage {
                input_tokens: 200,
                output_tokens: 40,
                ..LlmUsage::default()
            }),
            ..LlmFailure::provider_error("Upstream failure [provider_id]")
        }),
    ]));

    let events = drain(fx.live(chat_id, request("fail after work")).await).await;

    assert_eq!(names(&events), ["stream_started", "delta", "error"]);
    let error = error_of(&events);
    assert_eq!(error.code, "provider_error");
    assert_eq!(error.message, "Upstream failure [provider_id]");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(turn.assistant_message_id, None);
    assert!(turn.completed_at.is_some());
    assert_eq!(message_rows(&fx.store, chat_id).await.len(), 1);
    assert_settled(&fx.quota_rows().await, standard_credits(200, 40));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["billing_outcome"], "failed");
    assert_eq!(usage[0].payload["settlement_method"], "actual");
}

#[tokio::test]
async fn provider_failure_without_usage_settles_estimated() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider
        .enqueue(Script::Events(vec![LlmEvent::TextDelta("cut ".to_owned())]));

    let events = drain(fx.live(chat_id, request("drop me")).await).await;

    let error = error_of(&events);
    assert_eq!(error.code, "provider_error");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_settled(&fx.quota_rows().await, estimated_standard_charge(&turn));
}

#[tokio::test]
async fn rate_limited_before_the_stream_names_the_retry_delay() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Fail(LlmFailure {
        retry_after_secs: Some(7),
        ..LlmFailure::new(LlmFailureKind::RateLimited, "Rate limited by the provider")
    }));

    let events = drain(fx.live(chat_id, request("hurry")).await).await;

    assert_eq!(names(&events), ["stream_started", "error"]);
    let error = error_of(&events);
    assert_eq!(error.code, "rate_limited");
    assert!(error.message.contains('7'), "{error:?}");
    assert_eq!(
        fx.turn(chat_id).await.error_code.as_deref(),
        Some("rate_limited")
    );
}

#[tokio::test]
async fn adapter_failure_before_the_outbound_call_releases_the_reserve() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Fail(LlmFailure::not_sent(
        "Provider request could not be authorized",
    )));

    let events = drain(fx.live(chat_id, request("never sent")).await).await;

    assert_eq!(names(&events), ["stream_started", "error"]);
    let error = error_of(&events);
    assert_eq!(error.code, "provider_error");
    assert_eq!(error.message, "Provider request could not be authorized");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("turn_setup_failed"));
    assert_eq!(turn.assistant_message_id, None);
    let totals: Vec<quota_usage::Model> = fx
        .quota_rows()
        .await
        .into_iter()
        .filter(|row| row.bucket == "total")
        .collect();
    assert_eq!(totals.len(), 2, "{totals:?}");
    for row in &totals {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.spent_credits_micro, 0, "{row:?}");
    }
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].payload["terminal_state"], "failed");
    assert_eq!(usage[0].payload["billing_outcome"], "failed");
    assert_eq!(usage[0].payload["settlement_method"], "released");
    assert_eq!(usage[0].payload["actual_credits_micro"], 0);
    assert_eq!(usage[0].payload["usage"]["input_tokens"], 0);
    assert_eq!(usage[0].payload["usage"]["output_tokens"], 0);
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(audit[0].payload["error_code"], "turn_setup_failed");
}

#[tokio::test]
async fn web_search_start_above_the_limit_fails_the_turn_estimated() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let start = || LlmEvent::ToolStarted {
        name: "web_search".to_owned(),
        details: json!({}),
    };
    let done = || LlmEvent::ToolCompleted {
        name: "web_search".to_owned(),
        details: json!({}),
    };
    fx.provider.enqueue(Script::Hang(vec![
        start(),
        done(),
        start(),
        done(),
        start(),
        LlmEvent::TextDelta("never sent".to_owned()),
    ]));

    let events = drain(
        fx.live(
            chat_id,
            SendRequest {
                web_search_enabled: true,
                ..request("search a lot")
            },
        )
        .await,
    )
    .await;

    assert_eq!(
        names(&events),
        ["stream_started", "tool", "tool", "tool", "tool", "error"]
    );
    assert_eq!(error_of(&events).code, "web_search_calls_exceeded");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert_eq!(turn.web_search_completed_count, 2);
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["settlement_method"], "estimated");
    assert_eq!(usage[0].payload["billing_outcome"], "failed");
    assert_eq!(usage[0].payload["web_search_calls"], 2);
    let total_daily = fx
        .quota_rows()
        .await
        .into_iter()
        .find(|r| r.bucket == "total" && r.period_type == "daily")
        .unwrap();
    assert_eq!(total_daily.web_search_calls, 2);
}

#[tokio::test]
async fn function_call_without_knowledge_search_is_unexpected_tool_use() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let mut events = vec![
        LlmEvent::ToolStarted {
            name: "function_call".to_owned(),
            details: json!({ "index": 0, "call_id": "call_1", "name": "search_knowledge" }),
        },
        LlmEvent::ToolCompleted {
            name: "function_call".to_owned(),
            details: json!({ "call_id": "call_1", "name": "search_knowledge", "arguments": "{}" }),
        },
        LlmEvent::FunctionCall(crate::domain::llm::LlmFunctionCall {
            call_id: "call_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "{}".to_owned(),
        }),
    ];
    events.push(completion(Some((40, 3))));
    fx.provider.enqueue(Script::Events(events));

    let events = drain(fx.live(chat_id, request("look it up")).await).await;

    assert_eq!(names(&events), ["stream_started", "tool", "tool", "error"]);
    assert_eq!(error_of(&events).code, "unexpected_tool_use");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("unexpected_tool_use"));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["billing_outcome"], "failed");
    assert_eq!(usage[0].payload["settlement_method"], "actual");
    assert_eq!(usage[0].payload["usage"]["input_tokens"], 40);
}

#[tokio::test]
async fn tool_events_are_relayed_and_counters_persist_while_running() {
    let fx = Fixture::with_settings(|settings| settings.progress_interval = Duration::ZERO).await;
    let chat_id = fx.chat("premium").await;
    let feed = fx.provider.channel();
    let mut live = fx
        .live(
            chat_id,
            SendRequest {
                web_search_enabled: true,
                ..request("search once")
            },
        )
        .await;
    let started_turn = fx.turn(chat_id).await;

    feed.send(LlmEvent::ToolStarted {
        name: "web_search".to_owned(),
        details: json!({}),
    })
    .unwrap();
    feed.send(LlmEvent::ToolCompleted {
        name: "web_search".to_owned(),
        details: json!({}),
    })
    .unwrap();
    for phase in [ToolPhase::Start, ToolPhase::Done] {
        let event = tokio::time::timeout(Duration::from_secs(5), live.rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event,
            StreamEvent::Tool(crate::domain::stream::events::ToolActivity {
                phase,
                name: "web_search".to_owned(),
                details: json!({}),
            })
        );
    }
    let mut running = fx.turn(chat_id).await;
    for _ in 0..100 {
        if running.web_search_completed_count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        running = fx.turn(chat_id).await;
    }
    assert_eq!(running.state, "running");
    assert_eq!(running.web_search_completed_count, 1);
    assert!(running.last_progress_at.unwrap().at() >= started_turn.last_progress_at.unwrap().at());

    feed.send(LlmEvent::TextDelta("Found it.".to_owned()))
        .unwrap();
    feed.send(completion(Some((10, 5)))).unwrap();
    let rest = drain(live).await;
    assert_eq!(names(&rest).last(), Some(&"done"));
    assert_eq!(fx.turn(chat_id).await.web_search_completed_count, 1);
}

#[tokio::test]
async fn url_citations_are_sent_once_before_done_and_not_on_incomplete() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("premium").await;
    let annotation = LlmAnnotation {
        kind: LlmAnnotationKind::Url,
        url: Some("https://example.com/paris".to_owned()),
        title: Some("Paris".to_owned()),
        file_id: None,
        filename: None,
        start: Some(0),
        end: Some(5),
        snippet: "Paris".to_owned(),
    };
    fx.provider.enqueue(Script::Events(vec![
        LlmEvent::TextDelta("Paris is the capital.".to_owned()),
        LlmEvent::Annotation(annotation.clone()),
        completion(Some((10, 5))),
    ]));
    fx.provider.enqueue(Script::Events(vec![
        LlmEvent::TextDelta("Cut".to_owned()),
        LlmEvent::Annotation(annotation),
        LlmEvent::Completed {
            response_id: None,
            usage: Some(LlmUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..LlmUsage::default()
            }),
            incomplete_reason: Some("max_output_tokens".to_owned()),
            output_text: String::new(),
        },
    ]));

    let complete = drain(fx.live(chat_id, request("capital?")).await).await;
    let incomplete = drain(fx.live(chat_id, request("again?")).await).await;

    assert_eq!(
        names(&complete),
        ["stream_started", "delta", "citations", "done"]
    );
    let StreamEvent::Citations(items) = &complete[2] else {
        panic!("{complete:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, CitationSource::Web);
    assert_eq!(items[0].url.as_deref(), Some("https://example.com/paris"));
    assert_eq!(names(&incomplete), ["stream_started", "delta", "done"]);
    let turns = fx.turns(chat_id).await;
    assert!(
        turns
            .iter()
            .all(|t| t.state == "completed" && t.error_code.is_none())
    );
}

#[tokio::test]
async fn completed_stream_without_text_persists_an_empty_message() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    fx.provider
        .enqueue(Script::Events(vec![completion(Some((20, 0)))]));

    let live = fx.live(chat_id, request("say nothing")).await;
    let message_id = live.first.message_id;
    let events = drain(live).await;

    assert_eq!(names(&events), ["stream_started", "done"]);
    let assistant = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(assistant.id, message_id);
    assert_eq!(assistant.content, "");
}

#[tokio::test]
async fn assistant_message_persistence_failure_finalizes_failed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let feed = fx.provider.channel();
    let live = fx.live(chat_id, request("answer me")).await;
    insert_message_with_id(&fx, chat_id, live.first.message_id).await;

    feed.send(LlmEvent::TextDelta("lost".to_owned())).unwrap();
    feed.send(completion(Some((30, 6)))).unwrap();
    let events = drain(live).await;

    assert_eq!(error_of(&events).code, "message_persistence_failed");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(turn.assistant_message_id, None);
    assert_settled(&fx.quota_rows().await, standard_credits(30, 6));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["billing_outcome"], "failed");
    assert_eq!(usage[0].payload["settlement_method"], "actual");
}

// ── Thread summary scheduling ────────────────────────────────────────────

async fn seed_history(fx: &Fixture, chat_id: Uuid) -> Uuid {
    let request_id = Uuid::new_v4();
    seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed {
            request_id: Some(request_id),
            content: "Earlier question".to_owned(),
            ..MessageSeed::new(MessageRole::User, -300)
        },
    )
    .await;
    seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed {
            request_id: Some(request_id),
            content: "Earlier answer".to_owned(),
            ..MessageSeed::new(MessageRole::Assistant, -200)
        },
    )
    .await
}

#[tokio::test]
async fn summary_trigger_enqueues_the_frozen_target_of_earlier_messages() {
    let fx = Fixture::with_settings(|settings| settings.compression_threshold_pct = 0).await;
    let chat_id = fx.chat("standard").await;
    let target = seed_history(&fx, chat_id).await;

    let events = drain(fx.live(chat_id, request("next question")).await).await;

    assert_eq!(names(&events).last(), Some(&"done"));
    let delivered = fx.store.wait_delivered(&fx.queues.thread_summary, 1).await;
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    let payload = &delivered[0].payload;
    assert_eq!(payload["chat_id"], json!(chat_id));
    assert_eq!(payload["tenant_id"], json!(TENANT_A));
    assert_eq!(payload["system_task_type"], "thread_summary_update");
    assert_eq!(payload["frozen_target_message_id"], json!(target));
    assert_eq!(payload["base_frontier_message_id"], serde_json::Value::Null);
}

#[tokio::test]
async fn summary_trigger_without_earlier_messages_enqueues_nothing() {
    let fx = Fixture::with_settings(|settings| settings.compression_threshold_pct = 0).await;
    let chat_id = fx.chat("standard").await;

    drain(fx.live(chat_id, request("first question")).await).await;

    fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert!(fx.store.delivered(&fx.queues.thread_summary).is_empty());
}

#[tokio::test]
async fn existing_summary_is_applied_and_bounds_the_recent_messages() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let covered = seed_history(&fx, chat_id).await;
    let frontier_at = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.id == covered)
        .unwrap()
        .created_at;
    let model = thread_summaries::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        summary_text: Set("They talked about earlier things.".to_owned()),
        summarized_up_to_created_at: Set(frontier_at),
        summarized_up_to_message_id: Set(covered),
        token_estimate: Set(42),
        created_at: Set(fx.store.stamp(at(0))),
        updated_at: Set(fx.store.stamp(at(0))),
    };
    secure_insert::<thread_summaries::Entity>(model, &AccessScope::allow_all(), &fx.store.conn())
        .await
        .unwrap();

    let live = fx.live(chat_id, request("what now?")).await;
    assert_eq!(live.first.thread_summary_applied, Some(42));
    drain(live).await;

    let sent = &fx.provider.requests()[0];
    assert_eq!(sent.input.len(), 2, "{:?}", sent.input);
    let LlmInputItem::User { text, .. } = &sent.input[0] else {
        panic!("{:?}", sent.input);
    };
    assert!(
        text.ends_with("They talked about earlier things."),
        "{text}"
    );
}

#[tokio::test]
async fn chats_row_is_untouched_by_a_rejected_request() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat("standard").await;
    let before = chat_row(&fx.store, chat_id).await;

    fx.send(chat_id, request("")).await.unwrap_err();

    assert_eq!(chat_row(&fx.store, chat_id).await, before);
}
