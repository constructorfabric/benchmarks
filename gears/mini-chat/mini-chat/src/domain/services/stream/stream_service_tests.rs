#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, PublishError, TierLimits,
    UsageEvent, UsageTokens, UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::events::{
    Citation, CitationSource, DeltaKind, DoneData, StreamEvent, StreamStartedData,
    ThreadSummaryInfo, ToolPhase, UsageCounts,
};
use super::relay::relay;
use super::{SendMessage, StreamDeps, StreamService, StreamStart};
use crate::config::MiniChatConfig;
use crate::domain::error::{DomainError, QuotaScope, ResourceKind};
use crate::domain::ports::{
    ChatAction, CompletionResult, ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest,
    PolicyProvider, ProviderError, RawCitation, ResolvedProvider, ToolSpec,
};
use crate::domain::services::finalization_service::FinalizationService;
use crate::domain::services::model_service::ModelService;
use crate::domain::services::quota_service::{QuotaDecisionKind, QuotaService};
use crate::domain::services::turn_service::TurnService;
use crate::domain::time::db_now;
use crate::infra::db::entities::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, quota_usage,
    thread_summary,
};
use crate::infra::llm::ProviderResolver;
use crate::infra::outbox::USAGE_PAYLOAD_TYPE;
use crate::test_support::{
    FakeAuthz, FakePolicy, catalog_entry, ctx_for, insert_message, seed_attachment, seed_turn,
    snapshot, test_file_db, test_outbox,
};

// ── Fake LLM ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Step {
    Ev(LlmEvent),
    Sleep(Duration),
    /// Wait until the provider stream is cancelled.
    Hang,
}

fn completed(input: i64, output: i64) -> LlmEvent {
    LlmEvent::Completed {
        usage: Some(UsageTokens {
            input_tokens: input,
            output_tokens: output,
            ..UsageTokens::default()
        }),
        response_id: Some("resp_secret123".to_owned()),
        incomplete_reason: None,
    }
}

fn delta(s: &str) -> Step {
    Step::Ev(LlmEvent::TextDelta(s.to_owned()))
}

fn tool_start(name: &str) -> Step {
    Step::Ev(LlmEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    })
}

fn tool_done(name: &str) -> Step {
    Step::Ev(LlmEvent::ToolDone {
        name: name.to_owned(),
        details: json!({}),
    })
}

fn default_script() -> Vec<Step> {
    vec![delta("Hello"), delta(" world"), Step::Ev(completed(12, 5))]
}

/// Scripted [`LlmClient`]: one script per call (FIFO), the default script
/// ("Hello" + " world", usage 12/5) when the queue is empty.
#[derive(Default)]
struct FakeLlm {
    scripts: Mutex<VecDeque<Result<Vec<Step>, ProviderError>>>,
    calls: AtomicUsize,
    requests: Mutex<Vec<LlmRequest>>,
    provider_cancelled: Arc<AtomicBool>,
    tokens: Mutex<Vec<CancellationToken>>,
}

impl FakeLlm {
    fn push(&self, script: Vec<Step>) {
        self.scripts.lock().unwrap().push_back(Ok(script));
    }

    fn push_err(&self, e: ProviderError) {
        self.scripts.lock().unwrap().push_back(Err(e));
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> LlmRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }

    /// The provider stream of the last call was cancelled (token fired, or
    /// observed by a hanging step).
    fn cancelled(&self) -> bool {
        self.provider_cancelled.load(Ordering::SeqCst)
            || self
                .tokens
                .lock()
                .unwrap()
                .last()
                .is_some_and(CancellationToken::is_cancelled)
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn stream(
        &self,
        _provider: &ResolvedProvider,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req);
        self.tokens.lock().unwrap().push(cancel.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(default_script()))?;
        let flag = Arc::clone(&self.provider_cancelled);
        let state = (VecDeque::from(script), cancel, flag);
        Ok(Box::pin(futures::stream::unfold(
            state,
            |(mut steps, cancel, flag)| async move {
                loop {
                    match steps.pop_front()? {
                        Step::Ev(e) => return Some((e, (steps, cancel, flag))),
                        Step::Sleep(d) => {
                            tokio::select! {
                                () = tokio::time::sleep(d) => {}
                                () = cancel.cancelled() => {
                                    flag.store(true, Ordering::SeqCst);
                                    return None;
                                }
                            }
                        }
                        Step::Hang => {
                            cancel.cancelled().await;
                            flag.store(true, Ordering::SeqCst);
                            return None;
                        }
                    }
                }
            },
        )))
    }

    async fn complete(
        &self,
        _provider: &ResolvedProvider,
        _req: LlmRequest,
    ) -> Result<CompletionResult, ProviderError> {
        Err(ProviderError::provider(
            "complete is not used by the stream service",
        ))
    }
}

/// [`PolicyProvider`] over [`FakePolicy`] whose `snapshot` can be made to fail
/// (finalization failure).
struct FlakyPolicy {
    inner: FakePolicy,
    fail_snapshot: AtomicBool,
}

#[async_trait]
impl PolicyProvider for FlakyPolicy {
    async fn current(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        self.inner.current(user_id).await
    }

    async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        if self.fail_snapshot.load(Ordering::SeqCst) {
            return Err(DomainError::Internal("policy plugin down".to_owned()));
        }
        self.inner.snapshot(user_id, version).await
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        self.inner.user_limits(user_id, version).await
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        self.inner.publish_usage(ev).await
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

const MODEL: &str = "b";
/// Provider entry used as `knowledge_search.provider_id`.
const KB_PROVIDER: &str = "kb-provider";
const STANDARD: &str = "s";

fn big_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 1_000_000_000_000,
        limit_monthly_credits_micro: 10_000_000_000_000,
    }
}

fn standard_entry(id: &str) -> ModelCatalogEntry {
    let mut m = catalog_entry(id, true);
    m.tier = ModelTier::Standard;
    m
}

fn base_catalog() -> Vec<ModelCatalogEntry> {
    vec![catalog_entry(MODEL, true), standard_entry(STANDARD)]
}

struct Opts {
    catalog: Vec<ModelCatalogEntry>,
    kill: KillSwitches,
    limits: TierLimits,
    cfg: MiniChatConfig,
    chat_model: String,
    knowledge: Option<Arc<dyn crate::infra::llm::knowledge::KnowledgeRetriever>>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            catalog: base_catalog(),
            kill: snapshot(vec![]).kill_switches,
            limits: big_limits(),
            cfg: MiniChatConfig::default(),
            chat_model: MODEL.to_owned(),
            knowledge: None,
        }
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    svc: Arc<StreamService>,
    llm: Arc<FakeLlm>,
    authz: Arc<FakeAuthz>,
    policy: Arc<FlakyPolicy>,
    outbox_rx: UnboundedReceiver<(String, Value)>,
    ctx: SecurityContext,
    chat: chat::Model,
    turns_svc: Arc<TurnService>,
}

fn test_cfg(mut cfg: MiniChatConfig) -> MiniChatConfig {
    cfg.providers = serde_json::from_value(json!({
        "secret-provider": {"kind": "openai_responses", "host": "api.example.com"},
        KB_PROVIDER: {
            "kind": "openai_responses", "host": "kb.openai.azure.com",
            "storage_kind": "azure", "api_version": "2025-04-01-preview"
        }
    }))
    .unwrap();
    cfg.fill_upstream_aliases();
    cfg
}

async fn fx() -> Fx {
    fx_with(Opts::default()).await
}

async fn fx_with(o: Opts) -> Fx {
    let (dir, raw) = test_file_db().await;
    let (outbox, outbox_rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let cfg = Arc::new(test_cfg(o.cfg));
    let mut snap = snapshot(o.catalog);
    snap.kill_switches = o.kill;
    let policy = Arc::new(FlakyPolicy {
        inner: FakePolicy::with_limits(snap, o.limits, o.limits),
        fail_snapshot: AtomicBool::new(false),
    });
    let policy_dyn: Arc<dyn PolicyProvider> = policy.clone();
    let authz = Arc::new(FakeAuthz::default());
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&db),
        authz.clone(),
        Arc::clone(&policy_dyn),
        Arc::clone(&cfg),
    ));
    let finalization = Arc::new(FinalizationService::new(
        Arc::clone(&db),
        Arc::clone(&policy_dyn),
        Arc::clone(&quota),
        Arc::clone(&outbox),
    ));
    let turns = Arc::new(TurnService::new(Arc::clone(&db), authz.clone(), outbox));
    let models = Arc::new(ModelService::new(authz.clone(), Arc::clone(&policy_dyn)));
    let llm = Arc::new(FakeLlm::default());
    let svc = Arc::new(StreamService::new(StreamDeps {
        db: Arc::clone(&db),
        cfg: Arc::clone(&cfg),
        authz: authz.clone(),
        models,
        quota,
        finalization,
        llm: llm.clone(),
        resolver: Arc::new(ProviderResolver::new(&cfg)),
        turns: Arc::clone(&turns),
        knowledge: o.knowledge,
    }));
    let ctx = ctx_for(Uuid::new_v4(), Uuid::new_v4());
    let chat = seed_chat_model(&db, &ctx, &o.chat_model, db_now()).await;
    Fx {
        _dir: dir,
        db,
        svc,
        llm,
        authz,
        policy,
        outbox_rx,
        ctx,
        chat,
        turns_svc: turns,
    }
}

async fn seed_chat_model(
    db: &DBProvider<DomainError>,
    ctx: &SecurityContext,
    model: &str,
    updated_at: OffsetDateTime,
) -> chat::Model {
    let conn = db.conn().unwrap();
    secure_insert::<chat::Entity>(
        chat::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model.to_owned()),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(updated_at),
            updated_at: Set(updated_at),
            deleted_at: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap()
}

impl Fx {
    fn req(&self, content: &str) -> SendMessage {
        SendMessage {
            chat_id: self.chat.id,
            content: content.to_owned(),
            request_id: Some(Uuid::new_v4()),
            attachment_ids: vec![],
            web_search_enabled: false,
        }
    }

    async fn send(&self, req: SendMessage) -> Result<StreamStart, DomainError> {
        self.svc.send(self.ctx.clone(), req).await
    }

    async fn turns(&self) -> Vec<chat_turn::Model> {
        let conn = self.db.conn().unwrap();
        chat_turn::Entity::find()
            .filter(chat_turn::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    async fn turn(&self, request_id: Uuid) -> chat_turn::Model {
        self.turns()
            .await
            .into_iter()
            .find(|t| t.request_id == request_id)
            .expect("turn")
    }

    async fn messages(&self) -> Vec<message::Model> {
        let conn = self.db.conn().unwrap();
        let mut rows = message::Entity::find()
            .filter(message::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows
    }

    async fn quota_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.db.conn().unwrap();
        let mut rows = quota_usage::Entity::find()
            .filter(quota_usage::Column::TenantId.eq(self.ctx.subject_tenant_id()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap();
        rows.sort_by(|a, b| (&a.period_type, &a.bucket).cmp(&(&b.period_type, &b.bucket)));
        rows
    }

    async fn reserved_total(&self) -> i64 {
        self.quota_rows()
            .await
            .iter()
            .map(|r| r.reserved_credits_micro)
            .sum()
    }

    /// Waits for `n` outbox messages and returns them.
    async fn outbox(&mut self, n: usize) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while out.len() < n {
            let msg = tokio::time::timeout(Duration::from_secs(15), self.outbox_rx.recv())
                .await
                .expect("outbox message in time")
                .expect("outbox open");
            out.push(msg);
        }
        out
    }

    async fn usage_event(&mut self) -> Value {
        loop {
            let (ty, body) = self.outbox(1).await.remove(0);
            if ty == USAGE_PAYLOAD_TYPE {
                return body;
            }
        }
    }

    /// Asserts that no outbox message arrives within 1.5 s.
    async fn assert_outbox_quiet(&mut self) {
        let res = tokio::time::timeout(Duration::from_millis(1500), self.outbox_rx.recv()).await;
        assert!(res.is_err(), "unexpected outbox message: {res:?}");
    }

    async fn attachment(&self, kind: &str, status: &str, file_id: &str) -> attachment::Model {
        let a = seed_attachment(&self.db, &self.chat, kind, status, None).await;
        self.update_attachment(
            a.id,
            attachment::Column::ProviderFileId,
            Some(file_id.to_owned()),
        )
        .await;
        a
    }

    async fn update_attachment<V: Into<sea_orm::Value>>(
        &self,
        id: Uuid,
        col: attachment::Column,
        v: V,
    ) {
        let conn = self.db.conn().unwrap();
        attachment::Entity::update_many()
            .col_expr(col, Expr::value(v))
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    async fn vector_store(&self, vs_id: &str) {
        let conn = self.db.conn().unwrap();
        secure_insert::<chat_vector_store::Entity>(
            chat_vector_store::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.chat.tenant_id),
                chat_id: Set(self.chat.id),
                vector_store_id: Set(Some(vs_id.to_owned())),
                provider: Set("openai".to_owned()),
                file_count: Set(1),
                created_at: Set(db_now()),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    async fn set_turn_state(&self, turn_id: Uuid, state: &str) {
        let conn = self.db.conn().unwrap();
        chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::State, Expr::value(state))
            .filter(chat_turn::Column::Id.eq(turn_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    /// A completed turn with its user and assistant messages.
    async fn seed_completed_turn(&self, request_id: Uuid, text: &str, model: &str) -> Uuid {
        let user = insert_message(
            &self.db,
            message_am(&self.chat, request_id, "user", "q", None),
        )
        .await;
        let assistant = insert_message(
            &self.db,
            message_am(&self.chat, request_id, "assistant", text, Some(model)),
        )
        .await;
        let _ = user;
        let t = seed_turn(&self.db, &self.chat, request_id, "completed", db_now()).await;
        let conn = self.db.conn().unwrap();
        chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::AssistantMessageId,
                Expr::value(Some(assistant.id)),
            )
            .filter(chat_turn::Column::Id.eq(t.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
        assistant.id
    }
}

fn message_am(
    chat: &chat::Model,
    request_id: Uuid,
    role: &str,
    content: &str,
    model: Option<&str>,
) -> message::ActiveModel {
    let assistant = role == "assistant";
    message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(Some(request_id)),
        role: Set(role.to_owned()),
        content: Set(content.to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(1),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(if assistant { 30 } else { 0 }),
        output_tokens: Set(if assistant { 7 } else { 0 }),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(model.map(str::to_owned)),
        is_compressed: Set(false),
        created_at: Set(db_now()),
        deleted_at: Set(None),
    }
}

/// All events of a stream start (the live stream through the relay).
async fn collect(start: StreamStart) -> Vec<StreamEvent> {
    match start {
        StreamStart::Replay(evs) => evs,
        StreamStart::Live(rx, guard) => tokio::time::timeout(
            Duration::from_secs(20),
            relay(rx, guard).collect::<Vec<_>>(),
        )
        .await
        .expect("stream ends in time"),
    }
}

fn live(start: Result<StreamStart, DomainError>) -> StreamStart {
    match start {
        Ok(s @ StreamStart::Live(..)) => s,
        Ok(StreamStart::Replay(evs)) => panic!("expected a live stream, got replay {evs:?}"),
        Err(e) => panic!("expected a live stream, got {e:?}"),
    }
}

fn names(evs: &[StreamEvent]) -> Vec<&'static str> {
    evs.iter().map(StreamEvent::name).collect()
}

fn started(evs: &[StreamEvent]) -> StreamStartedData {
    match &evs[0] {
        StreamEvent::StreamStarted(d) => d.clone(),
        other => panic!("first event is {other:?}"),
    }
}

fn done(evs: &[StreamEvent]) -> DoneData {
    match evs.last() {
        Some(StreamEvent::Done(d)) => d.clone(),
        other => panic!("last event is {other:?}"),
    }
}

fn error_of(evs: &[StreamEvent]) -> (String, String) {
    match evs.last() {
        Some(StreamEvent::Error { code, message }) => (code.clone(), message.clone()),
        other => panic!("last event is {other:?}"),
    }
}

fn text_delta(s: &str) -> StreamEvent {
    StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: s.to_owned(),
    }
}

async fn wait_turn_state(fx: &Fx, request_id: Uuid, state: &str) -> chat_turn::Model {
    for _ in 0..200 {
        let t = fx.turn(request_id).await;
        if t.state == state {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("turn {request_id} did not reach {state}");
}

// ── Happy path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_event_order_and_persistence() {
    let mut fx = fx().await;
    let req = fx.req("hi");
    let request_id = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;

    assert_eq!(names(&evs), ["stream_started", "delta", "delta", "done"]);
    let s = started(&evs);
    assert_eq!(s.request_id, request_id);
    assert!(s.is_new_turn);
    assert_eq!(s.thread_summary_applied, None);
    assert_eq!(evs[1], text_delta("Hello"));
    assert_eq!(evs[2], text_delta(" world"));
    let d = done(&evs);
    assert_eq!(
        d.usage,
        UsageCounts {
            input_tokens: 12,
            output_tokens: 5
        }
    );
    assert_eq!(
        (d.effective_model.as_str(), d.selected_model.as_str()),
        (MODEL, MODEL)
    );
    assert_eq!(d.quota_decision, QuotaDecisionKind::Allow);
    assert_eq!((d.downgrade_from, d.downgrade_reason), (None, None));
    assert!(d.quota_warnings.is_some());

    let msgs = fx.messages().await;
    assert_eq!(msgs.len(), 2);
    let (user, assistant) = (&msgs[0], &msgs[1]);
    assert_eq!((user.role.as_str(), user.content.as_str()), ("user", "hi"));
    assert_eq!((user.token_estimate, assistant.token_estimate), (0, 0));
    assert_eq!(assistant.role, "assistant");
    assert_eq!(assistant.content, "Hello world");
    assert_eq!(assistant.id, s.message_id);
    assert_eq!(user.request_id, Some(request_id));
    assert_eq!(assistant.request_id, Some(request_id));

    let turn = fx.turn(request_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(s.message_id));
    assert_eq!(turn.effective_model.as_deref(), Some(MODEL));
    assert_eq!(turn.requester_user_id, Some(fx.ctx.subject_id()));
    assert!(turn.reserve_tokens.unwrap() > 0);
    assert_eq!(turn.max_output_tokens_applied, Some(4096));
    assert_eq!(turn.policy_version_applied, Some(1));
    assert!(!turn.web_search_enabled);
    assert_eq!(turn.provider_name, None, "reserved column, never populated");

    let usage = fx.usage_event().await;
    assert_eq!(usage["billing_outcome"], "completed");
    assert_eq!(usage["request_id"], request_id.to_string());
    assert_eq!(fx.reserved_total().await, 0);

    let r = fx.llm.last_request();
    assert_eq!(r.model, "prov-model-b");
    assert_eq!(r.user.len(), 64);
    assert_eq!(r.max_output_tokens, 4096);
    assert_eq!(r.metadata.request_type, "chat");
    assert_eq!(r.metadata.feature, "none");
    assert_eq!(r.metadata.chat_id, fx.chat.id.to_string());
    assert!(r.tools.is_empty());
    assert_eq!(
        r.input,
        vec![InputItem::Message {
            role: "user",
            content: vec![ContentPart::InputText("hi".to_owned())]
        }]
    );
    assert_eq!(fx.authz.chat_actions(), vec![ChatAction::SendMessage]);
}

#[tokio::test]
async fn chat_updated_at_bumped_on_send() {
    let fx = fx().await;
    let old = OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();
    let chat = seed_chat_model(&fx.db, &fx.ctx, MODEL, old).await;
    let mut req = fx.req("hi");
    req.chat_id = chat.id;
    let _ = collect(live(fx.send(req).await)).await;
    let conn = fx.db.conn().unwrap();
    let row = chat::Entity::find()
        .filter(chat::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert!(row.updated_at > old, "{} vs {old}", row.updated_at);
}

#[tokio::test]
async fn new_turn_allowed_after_terminal_and_history_is_sent() {
    let fx = fx().await;
    let _ = collect(live(fx.send(fx.req("first")).await)).await;
    let second = fx.req("second");
    let rid = second.request_id.unwrap();
    let evs = collect(live(fx.send(second).await)).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    assert_eq!(fx.turn(rid).await.state, "completed");
    assert_eq!(fx.turns().await.len(), 2);
    let r = fx.llm.last_request();
    assert_eq!(r.input.len(), 3, "{:?}", r.input);
    assert_eq!(
        r.input[1],
        InputItem::Message {
            role: "assistant",
            content: vec![ContentPart::OutputText("Hello world".to_owned())]
        }
    );
}

#[tokio::test]
async fn omitted_request_id_is_generated() {
    let fx = fx().await;
    let mut req = fx.req("hi");
    req.request_id = None;
    let evs = collect(live(fx.send(req).await)).await;
    let s = started(&evs);
    assert_eq!(fx.turn(s.request_id).await.state, "completed");
}

#[tokio::test]
async fn pings_only_before_first_content() {
    let mut o = Opts::default();
    o.cfg.streaming.sse_ping_interval_seconds = 5;
    let fx = fx_with(o).await;
    fx.llm.push(vec![
        Step::Sleep(Duration::from_millis(12_500)),
        delta("a"),
        Step::Sleep(Duration::from_secs(31)),
        delta("b"),
        Step::Ev(completed(1, 1)),
    ]);
    let start = live(fx.send(fx.req("hi")).await);
    tokio::time::pause();
    let evs = match start {
        StreamStart::Live(rx, guard) => relay(rx, guard).collect::<Vec<_>>().await,
        StreamStart::Replay(_) => unreachable!(),
    };
    tokio::time::resume();
    assert_eq!(
        names(&evs),
        ["stream_started", "ping", "ping", "delta", "delta", "done"]
    );
}

#[tokio::test]
async fn empty_deltas_skipped_and_second_terminal_ignored() {
    let fx = fx().await;
    fx.llm.push(vec![
        delta(""),
        delta("a"),
        Step::Ev(completed(1, 1)),
        Step::Ev(LlmEvent::Failed {
            error: ProviderError::provider("late"),
            usage: None,
        }),
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "done"]);
    assert_eq!(evs[1], text_delta("a"));
    assert_eq!(fx.turn(rid).await.state, "completed");
}

#[tokio::test]
async fn reasoning_delta_is_relayed_but_not_persisted() {
    let fx = fx().await;
    fx.llm.push(vec![
        Step::Ev(LlmEvent::ReasoningDelta("think".to_owned())),
        delta("answer"),
        Step::Ev(completed(1, 1)),
    ]);
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(
        evs[1],
        StreamEvent::Delta {
            kind: DeltaKind::Reasoning,
            content: "think".to_owned()
        }
    );
    assert_eq!(fx.messages().await[1].content, "answer");
}

// ── Idempotency and the parallel guard ───────────────────────────────────────

#[tokio::test]
async fn replay_is_side_effect_free() {
    let mut fx = fx().await;
    let req = fx.req("hi");
    let evs = collect(live(fx.send(req.clone()).await)).await;
    let message_id = started(&evs).message_id;
    let _ = fx.outbox(2).await; // usage + audit of the original turn
    let quota_before = fx.quota_rows().await;
    let calls = fx.llm.calls();

    let replay = fx.send(req.clone()).await.unwrap();
    let StreamStart::Replay(evs) = replay else {
        panic!("expected a replay");
    };
    assert_eq!(evs.len(), 3);
    assert_eq!(
        evs[0],
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: req.request_id.unwrap(),
            message_id,
            is_new_turn: false,
            thread_summary_applied: None,
        })
    );
    assert_eq!(evs[1], text_delta("Hello world"));
    assert_eq!(
        evs[2],
        StreamEvent::Done(DoneData {
            usage: UsageCounts {
                input_tokens: 12,
                output_tokens: 5
            },
            effective_model: MODEL.to_owned(),
            selected_model: MODEL.to_owned(),
            quota_decision: QuotaDecisionKind::Allow,
            downgrade_from: None,
            downgrade_reason: None,
            quota_warnings: None,
        })
    );
    assert_eq!(fx.llm.calls(), calls);
    assert_eq!(fx.quota_rows().await, quota_before);
    assert_eq!(fx.messages().await.len(), 2);
    fx.assert_outbox_quiet().await;
}

#[tokio::test]
async fn replay_of_downgraded_turn_rebuilds_decision_without_reason() {
    let fx = fx().await;
    let rid = Uuid::new_v4();
    fx.seed_completed_turn(rid, "old answer", STANDARD).await;
    let mut req = fx.req("hi");
    req.request_id = Some(rid);
    let StreamStart::Replay(evs) = fx.send(req).await.unwrap() else {
        panic!("expected a replay");
    };
    let d = done(&evs);
    assert_eq!(d.effective_model, STANDARD);
    assert_eq!(d.selected_model, MODEL);
    assert_eq!(d.quota_decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_from.as_deref(), Some(MODEL));
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(
        d.usage,
        UsageCounts {
            input_tokens: 30,
            output_tokens: 7
        }
    );
    assert_eq!(evs[1], text_delta("old answer"));
}

#[tokio::test]
async fn replay_checked_before_parallel_guard() {
    let fx = fx().await;
    let rid = Uuid::new_v4();
    fx.seed_completed_turn(rid, "done before", MODEL).await;
    seed_turn(&fx.db, &fx.chat, Uuid::new_v4(), "running", db_now()).await;
    let mut req = fx.req("hi");
    req.request_id = Some(rid);
    assert!(matches!(fx.send(req).await, Ok(StreamStart::Replay(_))));
    assert_eq!(fx.llm.calls(), 0);
}

#[tokio::test]
async fn request_id_conflict_for_failed_cancelled_running_and_deleted_turns() {
    let fx = fx().await;
    for state in ["failed", "cancelled", "running"] {
        let rid = Uuid::new_v4();
        let t = seed_turn(&fx.db, &fx.chat, rid, state, db_now()).await;
        let mut req = fx.req("hi");
        req.request_id = Some(rid);
        assert_eq!(
            fx.send(req).await.unwrap_err(),
            DomainError::RequestIdConflict,
            "{state}"
        );
        // Keep the chat free of running turns for the next case.
        fx.set_turn_state(t.id, "failed").await;
    }
    let rid = Uuid::new_v4();
    fx.seed_completed_turn(rid, "replaced", MODEL).await;
    let conn = fx.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(db_now())))
        .filter(chat_turn::Column::RequestId.eq(rid))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let mut req = fx.req("hi");
    req.request_id = Some(rid);
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::RequestIdConflict
    );
    assert_eq!(fx.llm.calls(), 0);
}

#[tokio::test]
async fn parallel_guard_rejects_second_running() {
    let fx = fx().await;
    seed_turn(&fx.db, &fx.chat, Uuid::new_v4(), "running", db_now()).await;
    assert_eq!(
        fx.send(fx.req("hi")).await.unwrap_err(),
        DomainError::TurnAlreadyRunning
    );
    assert_eq!(fx.llm.calls(), 0);
    assert_eq!(fx.reserved_total().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_one_wins() {
    let fx = fx().await;
    let slow = vec![
        Step::Sleep(Duration::from_secs(2)),
        delta("x"),
        Step::Ev(completed(1, 1)),
    ];
    fx.llm.push(slow.clone());
    fx.llm.push(slow);
    let (a, b) = (fx.req("one"), fx.req("two"));
    let (svc_a, svc_b) = (Arc::clone(&fx.svc), Arc::clone(&fx.svc));
    let (ctx_a, ctx_b) = (fx.ctx.clone(), fx.ctx.clone());
    let ha = tokio::spawn(async move { svc_a.send(ctx_a, a).await });
    let hb = tokio::spawn(async move { svc_b.send(ctx_b, b).await });
    let results = [ha.await.unwrap(), hb.await.unwrap()];
    let live_count = results
        .iter()
        .filter(|r| matches!(r, Ok(StreamStart::Live(..))))
        .count();
    let running_count = results
        .iter()
        .filter(|r| matches!(r, Err(DomainError::TurnAlreadyRunning)))
        .count();
    assert_eq!((live_count, running_count), (1, 1), "{results:?}");
    let users = fx
        .messages()
        .await
        .into_iter()
        .filter(|m| m.role == "user")
        .count();
    assert_eq!(users, 1);
    assert_eq!(fx.turns().await.len(), 1);
    for s in results.into_iter().flatten() {
        let evs = collect(s).await;
        assert_eq!(names(&evs).last(), Some(&"done"));
    }
    assert_eq!(fx.reserved_total().await, 0);
}

// ── Pre-stream validation ────────────────────────────────────────────────────

#[tokio::test]
async fn empty_content_rejected_before_authz() {
    let fx = fx().await;
    assert_eq!(
        fx.send(fx.req(" \n\t ")).await.unwrap_err(),
        DomainError::EmptyContent
    );
    assert!(fx.authz.chat_actions().is_empty());
}

#[tokio::test]
async fn duplicate_attachment_ids_invalid_attachment() {
    let fx = fx().await;
    let id = Uuid::new_v4();
    let mut req = fx.req("hi");
    req.attachment_ids = vec![id, id];
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::InvalidAttachment
    );
    assert!(fx.authz.chat_actions().is_empty());
}

#[tokio::test]
async fn too_many_attachment_ids_invalid_attachment() {
    let mut o = Opts::default();
    o.cfg.rag.max_documents_per_chat = 1;
    o.cfg.rag.max_images_per_message = 1;
    let fx = fx_with(o).await;
    let mut req = fx.req("hi");
    req.attachment_ids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::InvalidAttachment
    );
    assert!(fx.authz.chat_actions().is_empty());
}

#[tokio::test]
async fn unknown_chat_is_404() {
    let fx = fx().await;
    let mut req = fx.req("hi");
    req.chat_id = Uuid::new_v4();
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    );
}

#[tokio::test]
async fn removed_chat_model_is_invalid_model() {
    let fx = fx_with(Opts {
        chat_model: "gone".to_owned(),
        ..Opts::default()
    })
    .await;
    assert_eq!(
        fx.send(fx.req("hi")).await.unwrap_err(),
        DomainError::InvalidModel
    );
}

#[tokio::test]
async fn foreign_or_not_ready_attachment_rolls_back_reserve() {
    let fx = fx().await;
    let pending = fx
        .attachment("document", "pending", "file-pending0000000")
        .await;
    let foreign = fx
        .attachment("document", "ready", "file-foreign0000000")
        .await;
    fx.update_attachment(
        foreign.id,
        attachment::Column::UploadedByUserId,
        Uuid::new_v4(),
    )
    .await;
    let other_chat = seed_chat_model(&fx.db, &fx.ctx, MODEL, db_now()).await;
    let elsewhere = seed_attachment(&fx.db, &other_chat, "document", "ready", None).await;
    for id in [pending.id, foreign.id, elsewhere.id, Uuid::new_v4()] {
        let mut req = fx.req("hi");
        req.attachment_ids = vec![id];
        assert_eq!(
            fx.send(req).await.unwrap_err(),
            DomainError::InvalidAttachment
        );
    }
    assert_eq!(fx.reserved_total().await, 0);
    assert!(fx.messages().await.is_empty());
    assert!(fx.turns().await.is_empty());
    assert_eq!(fx.llm.calls(), 0);
}

#[tokio::test]
async fn valid_attachments_are_linked_to_the_user_message() {
    let fx = fx().await;
    let doc = fx
        .attachment("document", "ready", "file-doc00000000000")
        .await;
    let mut req = fx.req("hi");
    req.attachment_ids = vec![doc.id];
    let _ = collect(live(fx.send(req).await)).await;
    let user = fx.messages().await.remove(0);
    let conn = fx.db.conn().unwrap();
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::MessageId.eq(user.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].attachment_id, doc.id);
}

#[tokio::test]
async fn too_many_images() {
    let mut o = Opts::default();
    o.cfg.rag.max_images_per_message = 1;
    let fx = fx_with(o).await;
    let a = fx.attachment("image", "ready", "file-img10000000000").await;
    let b = fx.attachment("image", "ready", "file-img20000000000").await;
    let mut req = fx.req("hi");
    req.attachment_ids = vec![a.id, b.id];
    assert_eq!(fx.send(req).await.unwrap_err(), DomainError::TooManyImages);
    assert_eq!(fx.llm.calls(), 0);
}

#[tokio::test]
async fn image_is_sent_as_input_image() {
    let fx = fx().await;
    let img = fx.attachment("image", "ready", "file-img10000000000").await;
    let mut req = fx.req("look");
    req.attachment_ids = vec![img.id];
    let _ = collect(live(fx.send(req).await)).await;
    let r = fx.llm.last_request();
    assert_eq!(
        r.input.last().unwrap(),
        &InputItem::Message {
            role: "user",
            content: vec![
                ContentPart::InputText("look".to_owned()),
                ContentPart::InputImage {
                    file_id: "file-img10000000000".to_owned()
                }
            ]
        }
    );
}

#[tokio::test]
async fn vision_rejected_after_downgrade() {
    let mut no_vision = standard_entry(STANDARD);
    no_vision.multimodal_capabilities = vec![];
    let mut kill = snapshot(vec![]).kill_switches;
    kill.force_standard_tier = true;
    let fx = fx_with(Opts {
        catalog: vec![catalog_entry(MODEL, true), no_vision],
        kill,
        ..Opts::default()
    })
    .await;
    let img = fx.attachment("image", "ready", "file-img10000000000").await;
    let mut req = fx.req("look");
    req.attachment_ids = vec![img.id];
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::VisionNotSupported
    );
    assert_eq!(fx.reserved_total().await, 0);
}

#[tokio::test]
async fn disable_images_rejects() {
    let mut kill = snapshot(vec![]).kill_switches;
    kill.disable_images = true;
    let fx = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let img = fx.attachment("image", "ready", "file-img10000000000").await;
    let mut req = fx.req("look");
    req.attachment_ids = vec![img.id];
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::FeatureDisabled { subject: "images" }
    );
}

#[tokio::test]
async fn input_too_long() {
    let mut m = catalog_entry(MODEL, true);
    m.max_input_tokens = 50;
    let fx = fx_with(Opts {
        catalog: vec![m],
        ..Opts::default()
    })
    .await;
    assert_eq!(
        fx.send(fx.req(&"x".repeat(400))).await.unwrap_err(),
        DomainError::InputTooLong
    );
    assert!(fx.turns().await.is_empty());
}

#[tokio::test]
async fn context_budget_exceeded_400() {
    let mut m = catalog_entry(MODEL, true);
    m.context_window = 5000;
    m.max_input_tokens = 0;
    let fx = fx_with(Opts {
        catalog: vec![m],
        ..Opts::default()
    })
    .await;
    assert_eq!(
        fx.send(fx.req(&"x".repeat(3000))).await.unwrap_err(),
        DomainError::ContextBudgetExceeded
    );
    assert!(fx.turns().await.is_empty());
    assert_eq!(fx.reserved_total().await, 0);
}

#[tokio::test]
async fn web_search_kill_switch_400_no_stream() {
    let mut kill = snapshot(vec![]).kill_switches;
    kill.disable_web_search = true;
    let fx = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let mut req = fx.req("search");
    req.web_search_enabled = true;
    assert_eq!(
        fx.send(req).await.unwrap_err(),
        DomainError::FeatureDisabled {
            subject: "web_search"
        }
    );
    assert_eq!(fx.llm.calls(), 0);
    assert!(fx.turns().await.is_empty());
}

#[tokio::test]
async fn quota_exhausted_429_no_provider_call() {
    let fx = fx_with(Opts {
        limits: TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 1,
        },
        ..Opts::default()
    })
    .await;
    assert_eq!(
        fx.send(fx.req("hi")).await.unwrap_err(),
        DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }
    );
    assert_eq!(fx.llm.calls(), 0);
    assert!(fx.turns().await.is_empty());
    assert_eq!(fx.reserved_total().await, 0);
}

#[tokio::test]
async fn downgrade_is_reported_in_done() {
    let mut kill = snapshot(vec![]).kill_switches;
    kill.force_standard_tier = true;
    let fx = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    let d = done(&evs);
    assert_eq!(d.effective_model, STANDARD);
    assert_eq!(d.selected_model, MODEL);
    assert_eq!(d.quota_decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_from.as_deref(), Some(MODEL));
    assert_eq!(d.downgrade_reason.as_deref(), Some("force_standard_tier"));
    assert_eq!(fx.llm.last_request().model, "prov-model-s");
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_search_tool_included_only_when_supported() {
    let fx = fx().await;
    let mut req = fx.req("search");
    req.web_search_enabled = true;
    let rid = req.request_id.unwrap();
    let _ = collect(live(fx.send(req).await)).await;
    let r = fx.llm.last_request();
    assert!(
        r.tools
            .iter()
            .any(|t| matches!(t, ToolSpec::WebSearch { .. })),
        "{:?}",
        r.tools
    );
    let guard = MiniChatConfig::default().context.web_search_guard;
    assert!(r.instructions.contains(&guard), "{}", r.instructions);
    assert_eq!(r.metadata.feature, "web_search");
    assert!(fx.turn(rid).await.web_search_enabled);

    let mut unsupported = standard_entry(STANDARD);
    unsupported.general_config.tool_support.web_search = false;
    let fx2 = fx_with(Opts {
        catalog: vec![catalog_entry(MODEL, true), unsupported],
        chat_model: STANDARD.to_owned(),
        ..Opts::default()
    })
    .await;
    let mut req = fx2.req("search");
    req.web_search_enabled = true;
    let rid = req.request_id.unwrap();
    let _ = collect(live(fx2.send(req).await)).await;
    let r = fx2.llm.last_request();
    assert!(r.tools.is_empty(), "{:?}", r.tools);
    assert!(!r.instructions.contains(&guard));
    assert!(fx2.turn(rid).await.web_search_enabled);
}

#[tokio::test]
async fn web_search_limit_exceeded_fails_turn() {
    let mut o = Opts::default();
    o.cfg.quota.web_search_max_calls_per_message = 2;
    let mut fx = fx_with(o).await;
    fx.llm.push(vec![
        tool_start("web_search"),
        tool_done("web_search"),
        tool_start("web_search"),
        tool_done("web_search"),
        tool_start("web_search"),
        Step::Hang,
    ]);
    let mut req = fx.req("search");
    req.web_search_enabled = true;
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(
        names(&evs),
        ["stream_started", "tool", "tool", "tool", "tool", "error"]
    );
    assert_eq!(
        evs[1],
        StreamEvent::Tool {
            phase: ToolPhase::Start,
            name: "web_search".to_owned(),
            details: json!({})
        }
    );
    assert_eq!(error_of(&evs).0, "web_search_calls_exceeded");
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert_eq!(turn.web_search_completed_count, 2);
    let usage = fx.usage_event().await;
    assert_eq!(usage["billing_outcome"], "failed");
    assert_eq!(usage["settlement_method"], "estimated");
    assert!(fx.llm.cancelled());
}

#[tokio::test]
async fn code_interpreter_limit_exceeded_fails_turn() {
    let mut o = Opts::default();
    o.cfg.quota.code_interpreter_max_calls_per_message = 1;
    let fx = fx_with(o).await;
    fx.llm.push(vec![
        tool_start("code_interpreter"),
        tool_start("code_interpreter"),
        Step::Hang,
    ]);
    let req = fx.req("compute");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(error_of(&evs).0, "code_interpreter_calls_exceeded");
    assert_eq!(
        fx.turn(rid).await.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn tool_done_updates_turn_counts_while_running() {
    let fx = fx().await;
    fx.llm.push(vec![
        tool_start("file_search"),
        tool_done("file_search"),
        Step::Hang,
    ]);
    let req = fx.req("find");
    let rid = req.request_id.unwrap();
    let StreamStart::Live(mut rx, guard) = live(fx.send(req).await) else {
        unreachable!()
    };
    let mut seen = Vec::new();
    while seen.len() < 3 {
        seen.push(rx.recv().await.unwrap());
    }
    for _ in 0..100 {
        if fx.turn(rid).await.file_search_completed_count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t = fx.turn(rid).await;
    assert_eq!(t.state, "running");
    assert_eq!(t.file_search_completed_count, 1);
    drop(guard);
    drop(rx);
    wait_turn_state(&fx, rid, "cancelled").await;
}

#[tokio::test]
async fn file_citations_map_to_attachments() {
    let fx = fx().await;
    let doc = fx
        .attachment("document", "ready", "file-AAAAAAAAAAAAAAAA")
        .await;
    let deleted = fx
        .attachment("document", "ready", "file-DELDELDELDELDEL")
        .await;
    fx.update_attachment(deleted.id, attachment::Column::DeletedAt, Some(db_now()))
        .await;
    fx.vector_store("vs_secretsecretsecret").await;
    let file = |id: &str| RawCitation::File {
        provider_file_id: id.to_owned(),
        filename: Some("provider-name.txt".to_owned()),
    };
    fx.llm.push(vec![
        delta("see doc"),
        Step::Ev(LlmEvent::Citations(vec![
            file("file-AAAAAAAAAAAAAAAA"),
            file("file-UNKNOWNUNKNOWN"),
            file("file-DELDELDELDELDEL"),
            RawCitation::Web {
                url: "https://example.com/a".to_owned(),
                title: "Example".to_owned(),
                snippet: "see".to_owned(),
                span: Some((0, 3)),
            },
        ])),
        Step::Ev(completed(3, 3)),
    ]);
    let evs = collect(live(fx.send(fx.req("cite")).await)).await;
    assert_eq!(
        names(&evs),
        ["stream_started", "delta", "citations", "done"]
    );
    let StreamEvent::Citations(items) = &evs[2] else {
        unreachable!()
    };
    assert_eq!(
        items,
        &vec![
            Citation {
                source: CitationSource::File,
                title: doc.filename.clone(),
                url: None,
                attachment_id: Some(doc.id),
                snippet: String::new(),
                span: None,
            },
            Citation {
                source: CitationSource::Web,
                title: "Example".to_owned(),
                url: Some("https://example.com/a".to_owned()),
                attachment_id: None,
                snippet: "see".to_owned(),
                span: Some(super::events::TextSpan { start: 0, end: 3 }),
            },
        ]
    );
    let r = fx.llm.last_request();
    assert!(r.tools.iter().any(|t| matches!(
        t,
        ToolSpec::FileSearch { vector_store_ids, .. } if vector_store_ids == &vec!["vs_secretsecretsecret".to_owned()]
    )));
    let guard = MiniChatConfig::default().context.file_search_guard;
    assert!(r.instructions.contains(&guard));
}

#[tokio::test]
async fn incomplete_response_sends_no_citations() {
    let fx = fx().await;
    fx.llm.push(vec![
        delta("cut"),
        Step::Ev(LlmEvent::Citations(vec![RawCitation::Web {
            url: "https://example.com".to_owned(),
            title: "t".to_owned(),
            snippet: String::new(),
            span: None,
        }])),
        Step::Ev(LlmEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: Some("max_output_tokens".to_owned()),
        }),
    ]);
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "done"]);
    // No provider usage: `done.usage` falls back to zeros.
    assert_eq!(done(&evs).usage, UsageCounts::default());
}

// ── Provider failures, disconnects, finalization outcomes ────────────────────

#[tokio::test]
async fn provider_failed_event_is_terminal_error_sanitized() {
    let fx = fx().await;
    fx.llm.push(vec![
        delta("partial"),
        Step::Ev(LlmEvent::Failed {
            error: ProviderError::provider(
                "boom resp_abc123 see https://internal.example/x key sk-abcdefghijklmnop",
            ),
            usage: None,
        }),
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "error"]);
    let (code, message) = error_of(&evs);
    assert_eq!(code, "provider_error");
    for leak in ["resp_abc123", "https://", "sk-abcdef"] {
        assert!(!message.contains(leak), "{message}");
    }
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(
        fx.messages().await.len(),
        1,
        "no assistant message on failure"
    );
}

#[tokio::test]
async fn provider_open_error_is_sse_error() {
    let fx = fx().await;
    fx.llm.push_err(ProviderError {
        code: "rate_limited",
        message: "provider rate limit exceeded".to_owned(),
        context_length_exceeded: false,
        retry_after_secs: None,
    });
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(names(&evs), ["stream_started", "error"]);
    assert_eq!(error_of(&evs).0, "rate_limited");
    assert_eq!(
        fx.turn(rid).await.error_code.as_deref(),
        Some("rate_limited")
    );
}

#[tokio::test]
async fn stream_without_terminal_is_provider_error() {
    let fx = fx().await;
    fx.llm.push(vec![delta("x")]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "error"]);
    assert_eq!(error_of(&evs).0, "provider_error");
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
}

#[tokio::test]
async fn disconnect_cancels_and_settles_estimated() {
    let mut fx = fx().await;
    fx.llm.push(vec![delta("partial"), Step::Hang]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let StreamStart::Live(mut rx, guard) = live(fx.send(req).await) else {
        unreachable!()
    };
    assert_eq!(rx.recv().await.unwrap().name(), "stream_started");
    assert_eq!(rx.recv().await.unwrap(), text_delta("partial"));
    drop(rx);
    drop(guard);
    let turn = wait_turn_state(&fx, rid, "cancelled").await;
    let msgs = fx.messages().await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].content, "partial");
    assert_eq!(turn.assistant_message_id, Some(msgs[1].id));
    let usage = fx.usage_event().await;
    assert_eq!(usage["billing_outcome"], "aborted");
    assert_eq!(usage["settlement_method"], "estimated");
    assert!(fx.llm.cancelled());
}

#[tokio::test]
async fn dropped_receiver_alone_is_a_disconnect() {
    let fx = fx().await;
    fx.llm.push(vec![
        delta("a"),
        Step::Sleep(Duration::from_millis(300)),
        delta("b"),
        Step::Hang,
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let StreamStart::Live(mut rx, guard) = live(fx.send(req).await) else {
        unreachable!()
    };
    assert_eq!(rx.recv().await.unwrap().name(), "stream_started");
    drop(rx);
    wait_turn_state(&fx, rid, "cancelled").await;
    drop(guard);
}

#[tokio::test]
async fn cas_lost_relays_stream_interrupted() {
    let fx = fx().await;
    fx.llm.push(vec![
        Step::Sleep(Duration::from_millis(500)),
        delta("late"),
        Step::Ev(completed(1, 1)),
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let start = live(fx.send(req).await);
    // Another finalizer (the orphan watchdog) wins first.
    let turn = fx.turn(rid).await;
    fx.set_turn_state(turn.id, "failed").await;
    let evs = collect(start).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "error"]);
    assert_eq!(error_of(&evs).0, "stream_interrupted");
}

#[tokio::test]
async fn finalization_failure_reports_finalization_failed() {
    let fx = fx().await;
    fx.policy.fail_snapshot.store(true, Ordering::SeqCst);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(error_of(&evs).0, "finalization_failed");
    assert_eq!(fx.turn(rid).await.state, "running");
}

#[tokio::test]
async fn failed_stream_keeps_its_code_when_finalization_fails() {
    let fx = fx().await;
    fx.policy.fail_snapshot.store(true, Ordering::SeqCst);
    fx.llm.push(vec![Step::Ev(LlmEvent::Failed {
        error: ProviderError::provider("down"),
        usage: None,
    })]);
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(error_of(&evs).0, "provider_error");
}

#[tokio::test]
async fn message_persistence_failure_is_reported() {
    let fx = fx().await;
    fx.llm.push(vec![
        Step::Sleep(Duration::from_millis(300)),
        delta("answer"),
        Step::Ev(completed(2, 2)),
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let start = live(fx.send(req).await);
    // An assistant row with the same request id makes the finalization insert fail.
    insert_message(
        &fx.db,
        message_am(&fx.chat, rid, "assistant", "squatter", Some(MODEL)),
    )
    .await;
    let evs = collect(start).await;
    assert_eq!(error_of(&evs).0, "message_persistence_failed");
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
}

/// A user message + running turn for [`super::setup::commit_send`].
fn new_send(fx: &Fx, request_id: Uuid) -> super::setup::NewSend {
    use crate::domain::enums::MessageRole;
    use crate::domain::services::quota_service::{PeriodStarts, ReserveRequest};
    use crate::infra::db::repos::message_repo::NewMessage;
    use crate::infra::db::repos::turn_repo::{NewRunningTurn, TurnPreflight};

    let now = db_now();
    let (tenant_id, user_id) = (fx.chat.tenant_id, fx.ctx.subject_id());
    super::setup::NewSend {
        reserve: ReserveRequest {
            tenant_id,
            user_id,
            premium: true,
            reserved_credits_micro: 1000,
            periods: PeriodStarts::at(now),
            limits: UserLimits {
                user_id,
                policy_version: 1,
                standard: big_limits(),
                premium: big_limits(),
            },
        },
        message: NewMessage {
            id: Uuid::new_v4(),
            tenant_id,
            chat_id: fx.chat.id,
            request_id,
            role: MessageRole::User,
            content: "hi".to_owned(),
            request_kind: "chat".to_owned(),
            features_used: json!([]),
            provider_response_id: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: None,
            created_at: now,
        },
        attachment_ids: vec![],
        turn: NewRunningTurn {
            id: Uuid::new_v4(),
            tenant_id,
            chat_id: fx.chat.id,
            request_id,
            requester_user_id: user_id,
            preflight: Some(TurnPreflight {
                reserve_tokens: 10,
                max_output_tokens_applied: 5,
                reserved_credits_micro: 1000,
                policy_version_applied: 1,
                effective_model: MODEL.to_owned(),
                minimal_generation_floor_applied: 1,
            }),
            web_search_enabled: false,
            now,
        },
    }
}

#[tokio::test]
async fn commit_unique_violation_on_same_request_id_is_request_id_conflict() {
    let fx = fx().await;
    let rid = Uuid::new_v4();
    // Lost race: the key was taken after the idempotency lookup.
    seed_turn(&fx.db, &fx.chat, rid, "failed", db_now()).await;
    let res = super::setup::commit_send(&fx.db, &fx.svc.quota, new_send(&fx, rid)).await;
    assert_eq!(res.unwrap_err(), DomainError::RequestIdConflict);
    assert_eq!(fx.reserved_total().await, 0);
    assert!(fx.messages().await.is_empty());
}

#[tokio::test]
async fn commit_unique_violation_on_running_index_is_turn_already_running() {
    let fx = fx().await;
    // Lost race: another turn started after the parallel guard.
    seed_turn(&fx.db, &fx.chat, Uuid::new_v4(), "running", db_now()).await;
    let res = super::setup::commit_send(&fx.db, &fx.svc.quota, new_send(&fx, Uuid::new_v4())).await;
    assert_eq!(res.unwrap_err(), DomainError::TurnAlreadyRunning);
    assert_eq!(fx.reserved_total().await, 0);
    assert!(fx.messages().await.is_empty());
    assert_eq!(fx.turns().await.len(), 1);
}

// ── Context ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn summary_applied_reported_in_stream_started() {
    let fx = fx().await;
    let old_rid = Uuid::new_v4();
    insert_message(&fx.db, message_am(&fx.chat, old_rid, "user", "old q", None)).await;
    let covered = insert_message(
        &fx.db,
        message_am(&fx.chat, old_rid, "assistant", "old a", Some(MODEL)),
    )
    .await;
    let conn = fx.db.conn().unwrap();
    secure_insert::<thread_summary::Entity>(
        thread_summary::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(fx.chat.tenant_id),
            chat_id: Set(fx.chat.id),
            summary_text: Set("They talked about cats.".to_owned()),
            summarized_up_to_created_at: Set(covered.created_at),
            summarized_up_to_message_id: Set(covered.id),
            token_estimate: Set(42),
            created_at: Set(db_now()),
            updated_at: Set(db_now()),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(
        started(&evs).thread_summary_applied,
        Some(ThreadSummaryInfo { token_estimate: 42 })
    );
    let r = fx.llm.last_request();
    assert_eq!(r.input.len(), 2, "summary + current message: {:?}", r.input);
    let InputItem::Message { role, content } = &r.input[0] else {
        panic!("expected a message")
    };
    assert_eq!(*role, "user");
    let ContentPart::InputText(text) = &content[0] else {
        panic!("summary is input text")
    };
    assert!(text.ends_with("They talked about cats."), "{text}");
}

// ── Relay ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn relay_appends_stream_interrupted_without_terminal() {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let token = CancellationToken::new();
    tx.send(text_delta("a")).await.unwrap();
    drop(tx);
    let evs: Vec<_> = relay(rx, super::DisconnectGuard::new(&token))
        .collect()
        .await;
    assert_eq!(names(&evs), ["delta", "error"]);
    assert_eq!(error_of(&evs).0, "stream_interrupted");
    assert!(token.is_cancelled(), "guard dropped at the end");
}

#[tokio::test]
async fn relay_stops_after_terminal() {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let token = CancellationToken::new();
    tx.send(StreamEvent::error("provider_error", "x"))
        .await
        .unwrap();
    tx.send(text_delta("never")).await.unwrap();
    let evs: Vec<_> = relay(rx, super::DisconnectGuard::new(&token))
        .collect()
        .await;
    assert_eq!(names(&evs), ["error"]);
    drop(tx);
}

#[path = "mutation_tests.rs"]
mod mutation_tests;

#[path = "knowledge_loop_tests.rs"]
mod knowledge_loop_tests;
