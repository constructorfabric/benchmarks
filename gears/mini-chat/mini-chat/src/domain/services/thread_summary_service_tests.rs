#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use mini_chat_sdk::{ModelCatalogEntry, UsageTokens};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use super::{
    SummaryRunResult, ThreadSummaryDeps, ThreadSummaryService, build_user_prompt, parse_summary,
    token_estimate,
};
use crate::config::MiniChatConfig;
use crate::domain::enums::MessageRole;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    CompletionResult, ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, PolicyProvider,
    ProviderError, ResolvedProvider,
};
use crate::domain::services::model_service::ModelService;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{chat, message, thread_summary};
use crate::infra::llm::ProviderResolver;
use crate::infra::outbox::payloads::ThreadSummaryPayload;
use crate::infra::outbox::{ThreadSummaryHandler, USAGE_PAYLOAD_TYPE};
use crate::test_support::{
    FakeAuthz, FakePolicy, catalog_entry, ctx_for, insert_message, seed_chat, snapshot,
    test_file_db, test_outbox,
};

const SUMMARY_MODEL: &str = "gpt-4.1-mini";
const SYSTEM_SUBJECT: &str = "11111111-6a88-4768-9dfc-6bcd5187d9ed";
const GOOD_REPLY: &str = "<analysis>thinking</analysis>\n<summary>The user likes tea.</summary>";

// ── Fake LLM ─────────────────────────────────────────────────────────────────

type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Scripted `complete`: pops one result per call (FIFO; [`GOOD_REPLY`] with
/// usage 100/30 when empty). An optional hook runs inside the first call
/// (before it returns), to change the database while the call is in flight.
#[derive(Default)]
struct FakeLlm {
    results: Mutex<VecDeque<Result<CompletionResult, ProviderError>>>,
    requests: Mutex<Vec<LlmRequest>>,
    hook: Mutex<Option<Hook>>,
}

impl FakeLlm {
    fn push_ok(&self, text: &str, usage: Option<UsageTokens>) {
        self.results.lock().unwrap().push_back(Ok(CompletionResult {
            text: text.to_owned(),
            usage,
        }));
    }

    fn push_err(&self, e: ProviderError) {
        self.results.lock().unwrap().push_back(Err(e));
    }

    fn set_hook(&self, hook: Hook) {
        *self.hook.lock().unwrap() = Some(hook);
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn stream(
        &self,
        _provider: &ResolvedProvider,
        _req: LlmRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderError> {
        Err(ProviderError::provider(
            "stream is not used by the summary worker",
        ))
    }

    async fn complete(
        &self,
        _provider: &ResolvedProvider,
        req: LlmRequest,
    ) -> Result<CompletionResult, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook.await;
        }
        let next = self.results.lock().unwrap().pop_front();
        next.unwrap_or_else(|| {
            Ok(CompletionResult {
                text: GOOD_REPLY.to_owned(),
                usage: Some(usage(100, 30, 0)),
            })
        })
    }
}

fn usage(input: i64, output: i64, reasoning: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        ..UsageTokens::default()
    }
}

fn ptl_error() -> ProviderError {
    ProviderError {
        context_length_exceeded: true,
        ..ProviderError::provider("prompt too long")
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    llm: Arc<FakeLlm>,
    svc: Arc<ThreadSummaryService>,
    rx: UnboundedReceiver<(String, Value)>,
    chat: chat::Model,
    t0: OffsetDateTime,
}

fn summary_entry() -> ModelCatalogEntry {
    catalog_entry(SUMMARY_MODEL, true)
}

async fn fx() -> Fx {
    fx_with(vec![summary_entry()], MiniChatConfig::default()).await
}

async fn fx_with(catalog: Vec<ModelCatalogEntry>, mut cfg: MiniChatConfig) -> Fx {
    let (dir, raw) = test_file_db().await;
    let (outbox, rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    cfg.providers = serde_json::from_value(json!({
        "secret-provider": {"kind": "openai_responses", "host": "api.example.com"}
    }))
    .unwrap();
    cfg.fill_upstream_aliases();
    let cfg = Arc::new(cfg);
    let policy: Arc<dyn PolicyProvider> = Arc::new(FakePolicy::new(snapshot(catalog)));
    let models = Arc::new(ModelService::new(Arc::new(FakeAuthz::default()), policy));
    let llm = Arc::new(FakeLlm::default());
    let svc = Arc::new(ThreadSummaryService::new(ThreadSummaryDeps {
        db: Arc::clone(&db),
        cfg: Arc::clone(&cfg),
        models,
        llm: llm.clone(),
        resolver: Arc::new(ProviderResolver::new(&cfg)),
        outbox,
    }));
    let ctx = ctx_for(Uuid::new_v4(), Uuid::new_v4());
    let t0 = db_ts(OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap());
    let chat = seed_chat(&db, &ctx, None, t0).await;
    Fx {
        _dir: dir,
        db,
        llm,
        svc,
        rx,
        chat,
        t0,
    }
}

impl Fx {
    /// Inserts a live message `sec` seconds after `t0`.
    async fn msg(&self, role: &str, content: &str, sec: i64) -> message::Model {
        insert_message(
            &self.db,
            message::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.chat.tenant_id),
                chat_id: Set(self.chat.id),
                request_id: Set(Some(Uuid::new_v4())),
                role: Set(role.to_owned()),
                content: Set(content.to_owned()),
                content_type: Set("text".to_owned()),
                token_estimate: Set(1),
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
                created_at: Set(db_ts(self.t0 + time::Duration::seconds(sec))),
                deleted_at: Set(None),
            },
        )
        .await
    }

    /// Alternating user / assistant messages `"{prefix}{i}"`, two seconds
    /// apart starting at `from`.
    async fn conversation(&self, prefix: &str, n: usize, from: i64) -> Vec<message::Model> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let sec = from + 2 * i64::try_from(i).unwrap();
            out.push(self.msg(role, &format!("{prefix}{i}"), sec).await);
        }
        out
    }

    fn payload(
        &self,
        base: Option<&message::Model>,
        target: &message::Model,
    ) -> ThreadSummaryPayload {
        ThreadSummaryPayload {
            tenant_id: self.chat.tenant_id,
            chat_id: self.chat.id,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: base.map(|m| m.created_at),
            base_frontier_message_id: base.map(|m| m.id),
            frozen_target_created_at: target.created_at,
            frozen_target_message_id: target.id,
            system_task_type: "thread_summary_update".to_owned(),
        }
    }

    /// Stores a summary row with frontier `upto` and marks every message up
    /// to it compressed (what a previous successful run leaves behind).
    async fn seed_summary(&self, text: &str, upto: &message::Model) {
        let conn = self.db.conn().unwrap();
        secure_insert::<thread_summary::Entity>(
            thread_summary::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.chat.tenant_id),
                chat_id: Set(self.chat.id),
                summary_text: Set(text.to_owned()),
                summarized_up_to_created_at: Set(upto.created_at),
                summarized_up_to_message_id: Set(upto.id),
                token_estimate: Set(7),
                created_at: Set(self.t0),
                updated_at: Set(self.t0),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
        message::Entity::update_many()
            .col_expr(message::Column::IsCompressed, Expr::value(true))
            .filter(message::Column::ChatId.eq(self.chat.id))
            .filter(message::Column::CreatedAt.lte(upto.created_at))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    async fn summary_row(&self) -> Option<thread_summary::Model> {
        let conn = self.db.conn().unwrap();
        thread_summary::Entity::find()
            .filter(thread_summary::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
    }

    /// `content -> is_compressed` of every message of the chat (live or not).
    async fn compressed(&self) -> Vec<(String, bool)> {
        let conn = self.db.conn().unwrap();
        let mut rows = message::Entity::find()
            .filter(message::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows.into_iter()
            .map(|m| (m.content, m.is_compressed))
            .collect()
    }

    /// The next delivered usage event (waits up to 10 s).
    async fn usage_event(&mut self) -> Value {
        loop {
            let (ty, body) = tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
                .await
                .expect("a usage event in time")
                .expect("outbox channel open");
            if ty == USAGE_PAYLOAD_TYPE {
                return body;
            }
        }
    }

    /// No usage event is delivered within 500 ms.
    async fn assert_no_usage_event(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while let Ok(Some((ty, body))) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            assert_ne!(ty, USAGE_PAYLOAD_TYPE, "unexpected usage event {body}");
        }
    }
}

fn soft_delete(db: Arc<DBProvider<DomainError>>, id: Uuid) -> Hook {
    Box::pin(async move {
        let conn = db.conn().unwrap();
        message::Entity::update_many()
            .col_expr(
                message::Column::DeletedAt,
                Expr::value(Some(db_ts(OffsetDateTime::now_utc()))),
            )
            .filter(message::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    })
}

fn prompt_of(req: &LlmRequest) -> String {
    assert_eq!(req.input.len(), 1, "one user message");
    let InputItem::Message { role, content } = &req.input[0] else {
        panic!("expected a message")
    };
    assert_eq!(*role, "user");
    match content.as_slice() {
        [ContentPart::InputText(t)] => t.clone(),
        other => panic!("unexpected content {other:?}"),
    }
}

fn is_done(r: &SummaryRunResult) -> bool {
    matches!(r, SummaryRunResult::Done)
}

// ── Run: commit ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn summarizes_exact_range_and_marks_compressed() {
    let mut f = fx().await;
    let m = f.conversation("m", 7, 1).await; // m0..m6
    f.seed_summary("Earlier summary.", &m[1]).await; // m0, m1 compressed
    let deleted = f.msg("user", "gone", 6).await; // between m2 and m3, soft-deleted
    soft_delete(Arc::clone(&f.db), deleted.id).await;
    let p = f.payload(Some(&m[1]), &m[4]);

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");

    let sent = f.llm.requests();
    assert_eq!(sent.len(), 1);
    let prompt = prompt_of(&sent[0]);
    for c in ["m2", "m3", "m4"] {
        assert!(prompt.contains(c), "{c} missing from {prompt}");
    }
    for c in ["m0", "m1", "m5", "m6", "gone"] {
        assert!(!prompt.contains(c), "{c} must not be in {prompt}");
    }

    let row = f.summary_row().await.expect("summary row");
    assert_eq!(row.summary_text, "The user likes tea.");
    assert_eq!(
        (
            row.summarized_up_to_created_at,
            row.summarized_up_to_message_id
        ),
        (m[4].created_at, m[4].id)
    );
    assert_eq!(row.token_estimate, 30);
    assert_eq!(
        f.compressed().await,
        vec![
            ("m0".to_owned(), true),
            ("m1".to_owned(), true),
            ("m2".to_owned(), true),
            ("gone".to_owned(), false),
            ("m3".to_owned(), true),
            ("m4".to_owned(), true),
            ("m5".to_owned(), false),
            ("m6".to_owned(), false),
        ]
    );
    let ev = f.usage_event().await;
    assert_eq!(ev["request_id"], p.system_request_id.to_string());
}

#[tokio::test]
async fn first_summary_without_base_inserts_row() {
    let mut f = fx().await;
    let m = f.conversation("m", 4, 1).await;
    let p = f.payload(None, &m[2]);

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");

    let row = f.summary_row().await.expect("summary row");
    assert_eq!(row.summarized_up_to_message_id, m[2].id);
    assert_eq!(
        f.compressed().await,
        vec![
            ("m0".to_owned(), true),
            ("m1".to_owned(), true),
            ("m2".to_owned(), true),
            ("m3".to_owned(), false),
        ]
    );
    f.usage_event().await;
}

#[tokio::test]
async fn system_usage_event_shape() {
    let mut f = fx().await;
    let m = f.conversation("m", 3, 1).await;
    let p = f.payload(None, &m[1]);
    f.llm.push_ok(
        GOOD_REPLY,
        Some(UsageTokens {
            input_tokens: 500,
            output_tokens: 40,
            cache_read_input_tokens: 100,
            cache_write_input_tokens: 0,
            reasoning_tokens: 10,
        }),
    );

    assert!(is_done(&f.svc.run(&p, 0).await));

    let ev = f.usage_event().await;
    let tenant_hex = f.chat.tenant_id.simple().to_string();
    let req_hex = p.system_request_id.simple().to_string();
    assert_eq!(ev["tenant_id"], f.chat.tenant_id.to_string());
    assert_eq!(ev["chat_id"], f.chat.id.to_string());
    assert_eq!(ev["request_id"], p.system_request_id.to_string());
    assert_eq!(ev["effective_model"], SUMMARY_MODEL);
    assert_eq!(ev["selected_model"], SUMMARY_MODEL);
    assert_eq!(ev["terminal_state"], "completed");
    assert_eq!(ev["billing_outcome"], "system_task");
    assert_eq!(ev["settlement_method"], "none");
    assert_eq!(ev["actual_credits_micro"], 0);
    assert_eq!(ev["requester_type"], "system");
    assert_eq!(ev["system_task_type"], "thread_summary_update");
    assert_eq!(
        ev["dedupe_key"],
        format!("{tenant_hex}/thread_summary_update/{req_hex}")
    );
    assert_eq!(
        ev["usage"],
        json!({
            "input_tokens": 500, "output_tokens": 40, "cache_read_input_tokens": 100,
            "cache_write_input_tokens": 0, "reasoning_tokens": 10
        })
    );
    assert_eq!(ev["web_search_calls"], 0);
    assert_eq!(ev["file_search_calls"], 0);
    assert_eq!(ev["code_interpreter_calls"], 0);
    let obj = ev.as_object().unwrap();
    assert!(!obj.contains_key("user_id"), "{ev}");
    assert!(!obj.contains_key("turn_id"), "{ev}");
    // output 40 - reasoning 10.
    assert_eq!(f.summary_row().await.unwrap().token_estimate, 30);
}

#[tokio::test]
async fn summary_request_uses_system_identity_and_catalog_limits() {
    let mut entry = summary_entry();
    entry.max_output_tokens = 777;
    entry.thread_summary_prompt = "Catalog summary prompt.".to_owned();
    let f = fx_with(vec![entry], MiniChatConfig::default()).await;
    let m = f.conversation("m", 3, 1).await;

    assert!(is_done(&f.svc.run(&f.payload(None, &m[1]), 0).await));

    let req = &f.llm.requests()[0];
    let tenant = f.chat.tenant_id;
    assert_eq!(req.model, format!("prov-model-{SUMMARY_MODEL}"));
    assert_eq!(req.max_output_tokens, 777);
    assert_eq!(req.instructions, "Catalog summary prompt.");
    assert!(req.tools.is_empty());
    assert!(!req.stream);
    assert_eq!(
        req.user,
        format!("{}{}", tenant.simple(), SYSTEM_SUBJECT.replace('-', ""))
    );
    assert_eq!(req.metadata.request_type, "summary");
    assert_eq!(req.metadata.feature, "none");
    assert_eq!(req.metadata.tenant_id, tenant.to_string());
    assert_eq!(req.metadata.user_id, SYSTEM_SUBJECT);
    assert_eq!(req.metadata.chat_id, f.chat.id.to_string());
}

#[tokio::test]
async fn system_prompt_falls_back_to_config() {
    let mut cfg = MiniChatConfig::default();
    cfg.thread_summary_worker.summary_system_prompt = "Config prompt.".to_owned();
    let f = fx_with(vec![summary_entry()], cfg).await;
    let m = f.conversation("m", 3, 1).await;

    assert!(is_done(&f.svc.run(&f.payload(None, &m[1]), 0).await));

    assert_eq!(f.llm.requests()[0].instructions, "Config prompt.");
}

// ── Run: skips ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn cas_conflict_finishes_without_commit() {
    let mut f = fx().await;
    let m = f.conversation("m", 5, 1).await;
    let p = f.payload(None, &m[2]);
    // Another run commits a summary while this one waits for the provider.
    let (db, tenant, chat_id, upto, t0) = (
        Arc::clone(&f.db),
        f.chat.tenant_id,
        f.chat.id,
        m[3].clone(),
        f.t0,
    );
    f.llm.set_hook(Box::pin(async move {
        let conn = db.conn().unwrap();
        secure_insert::<thread_summary::Entity>(
            thread_summary::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(tenant),
                chat_id: Set(chat_id),
                summary_text: Set("winner".to_owned()),
                summarized_up_to_created_at: Set(upto.created_at),
                summarized_up_to_message_id: Set(upto.id),
                token_estimate: Set(3),
                created_at: Set(t0),
                updated_at: Set(t0),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }));

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");

    let row = f.summary_row().await.unwrap();
    assert_eq!(row.summary_text, "winner");
    assert_eq!(row.summarized_up_to_message_id, m[3].id);
    assert!(f.compressed().await.iter().all(|(_, c)| !c));
    f.assert_no_usage_event().await;
}

#[tokio::test]
async fn advanced_frontier_skips_before_the_provider_call() {
    let mut f = fx().await;
    let m = f.conversation("m", 5, 1).await;
    f.seed_summary("newer", &m[3]).await;
    let p = f.payload(Some(&m[0]), &m[2]); // stale base

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");
    assert!(f.llm.requests().is_empty());
    assert_eq!(f.summary_row().await.unwrap().summary_text, "newer");
    f.assert_no_usage_event().await;
}

#[tokio::test]
async fn base_missing_skips() {
    let mut f = fx().await;
    let m = f.conversation("m", 5, 1).await;
    // The summary the task was based on was deleted by a turn mutation.
    let p = f.payload(Some(&m[0]), &m[2]);

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");
    assert!(f.llm.requests().is_empty());
    assert!(f.summary_row().await.is_none());
    assert!(f.compressed().await.iter().all(|(_, c)| !c));
    f.assert_no_usage_event().await;
}

#[tokio::test]
async fn frontier_deleted_skips() {
    let mut f = fx().await;
    let m = f.conversation("m", 5, 1).await;
    let p = f.payload(None, &m[3]);
    // A retry / edit / delete removes the target while the provider runs.
    f.llm.set_hook(soft_delete(Arc::clone(&f.db), m[3].id));

    let res = f.svc.run(&p, 0).await;
    assert!(is_done(&res), "{res:?}");
    assert_eq!(f.llm.requests().len(), 1);
    assert!(f.summary_row().await.is_none());
    assert!(f.compressed().await.iter().all(|(_, c)| !c));
    f.assert_no_usage_event().await;
}

#[tokio::test]
async fn frontier_already_deleted_skips_without_provider_call() {
    let f = fx().await;
    let m = f.conversation("m", 5, 1).await;
    soft_delete(Arc::clone(&f.db), m[3].id).await;

    let res = f.svc.run(&f.payload(None, &m[3]), 0).await;
    assert!(is_done(&res), "{res:?}");
    assert!(f.llm.requests().is_empty());
    assert!(f.summary_row().await.is_none());
}

// ── Run: failures ────────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_model_rejects() {
    let f = fx_with(
        vec![catalog_entry("other", true)],
        MiniChatConfig::default(),
    )
    .await;
    let m = f.conversation("m", 3, 1).await;

    let res = f.svc.run(&f.payload(None, &m[1]), 0).await;
    assert!(matches!(res, SummaryRunResult::Reject(_)), "{res:?}");
    assert!(f.llm.requests().is_empty());
    assert!(f.summary_row().await.is_none());
}

#[tokio::test]
async fn disabled_model_rejects() {
    let f = fx_with(
        vec![catalog_entry(SUMMARY_MODEL, false)],
        MiniChatConfig::default(),
    )
    .await;
    let m = f.conversation("m", 3, 1).await;

    let res = f.svc.run(&f.payload(None, &m[1]), 0).await;
    assert!(matches!(res, SummaryRunResult::Reject(_)), "{res:?}");
    assert!(f.llm.requests().is_empty());
}

#[tokio::test]
async fn empty_summary_retries_then_rejects_at_max() {
    let mut cfg = MiniChatConfig::default();
    cfg.thread_summary_worker.max_attempts = 3;
    let mut f = fx_with(vec![summary_entry()], cfg).await;
    let m = f.conversation("m", 3, 1).await;
    let p = f.payload(None, &m[1]);
    for _ in 0..3 {
        f.llm.push_ok("<analysis>only thinking</analysis>", None);
    }

    let first = f.svc.run(&p, 0).await;
    assert!(matches!(first, SummaryRunResult::Retry(_)), "{first:?}");
    let second = f.svc.run(&p, 1).await;
    assert!(matches!(second, SummaryRunResult::Retry(_)), "{second:?}");
    // The third delivery (attempts = 2) is the max_attempts-th.
    let third = f.svc.run(&p, 2).await;
    assert!(matches!(third, SummaryRunResult::Reject(_)), "{third:?}");

    assert!(f.summary_row().await.is_none());
    assert!(f.compressed().await.iter().all(|(_, c)| !c));
    f.assert_no_usage_event().await;
}

#[tokio::test]
async fn provider_error_retries_and_keeps_state() {
    let f = fx().await;
    let m = f.conversation("m", 3, 1).await;
    f.llm.push_err(ProviderError::timeout("slow"));

    let res = f.svc.run(&f.payload(None, &m[1]), 0).await;
    assert!(matches!(res, SummaryRunResult::Retry(_)), "{res:?}");
    assert!(f.summary_row().await.is_none());
}

#[tokio::test]
async fn context_length_error_drops_oldest_and_retries_twice() {
    let f = fx().await;
    let m = f.conversation("m", 10, 1).await;
    f.llm.push_err(ptl_error());
    f.llm.push_err(ptl_error());

    let res = f.svc.run(&f.payload(None, &m[9]), 0).await;
    assert!(is_done(&res), "{res:?}");

    let sent = f.llm.requests();
    assert_eq!(sent.len(), 3);
    let p0 = prompt_of(&sent[0]);
    let p1 = prompt_of(&sent[1]);
    let p2 = prompt_of(&sent[2]);
    // 10 messages, then ceil(10/5) = 2 dropped, then ceil(8/5) = 2 more.
    assert!(p0.contains("User: m0") && p0.contains("User: m2"));
    assert!(!p1.contains("User: m0") && !p1.contains("Assistant: m1"));
    assert!(p1.contains("User: m2"));
    assert!(!p2.contains("User: m2") && !p2.contains("Assistant: m3"));
    assert!(p2.contains("User: m4") && p2.contains("Assistant: m9"));
    // The whole frozen range is still marked compressed.
    let row = f.summary_row().await.unwrap();
    assert_eq!(row.summarized_up_to_message_id, m[9].id);
}

#[tokio::test]
async fn context_length_error_gives_up_after_two_retries() {
    let f = fx().await;
    let m = f.conversation("m", 10, 1).await;
    for _ in 0..3 {
        f.llm.push_err(ptl_error());
    }

    let res = f.svc.run(&f.payload(None, &m[9]), 0).await;
    assert!(matches!(res, SummaryRunResult::Retry(_)), "{res:?}");
    assert_eq!(f.llm.requests().len(), 3);
    assert!(f.summary_row().await.is_none());
}

#[tokio::test]
async fn prompt_is_fitted_to_the_summary_model_budget() {
    // Budget 6000 - 1000 = 5000 tokens at 4 bytes per token. Each entry is
    // ~1000 tokens: 10 -> 8 -> 6 -> 4 messages (ceil(n/5) per step).
    let mut entry = summary_entry();
    entry.context_window = 6000;
    entry.max_output_tokens = 1000;
    entry.max_input_tokens = 0;
    let f = fx_with(vec![entry], MiniChatConfig::default()).await;
    let mut m = Vec::new();
    for i in 0..10_i64 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let content = format!("#{i}#{}", "x".repeat(3990));
        m.push(f.msg(role, &content, i + 1).await);
    }

    assert!(is_done(&f.svc.run(&f.payload(None, &m[9]), 0).await));

    let prompt = prompt_of(&f.llm.requests()[0]);
    for i in 0..6 {
        assert!(!prompt.contains(&format!("#{i}#")), "#{i}# kept");
    }
    for i in 6..10 {
        assert!(prompt.contains(&format!("#{i}#")), "#{i}# dropped");
    }
}

#[tokio::test]
async fn fitting_keeps_at_least_two_messages() {
    let mut entry = summary_entry();
    entry.context_window = 1100;
    entry.max_output_tokens = 1000;
    entry.max_input_tokens = 0;
    let f = fx_with(vec![entry], MiniChatConfig::default()).await;
    let m = f.conversation("m", 6, 1).await;

    assert!(is_done(&f.svc.run(&f.payload(None, &m[5]), 0).await));

    let prompt = prompt_of(&f.llm.requests()[0]);
    assert!(prompt.contains("User: m4") && prompt.contains("Assistant: m5"));
    assert!(!prompt.contains("Assistant: m3"));
}

// ── Outbox handler ───────────────────────────────────────────────────────────

fn outbox_msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: "mini-chat.thread_summary.v1".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

#[tokio::test]
async fn handler_rejects_malformed_payload() {
    let f = fx().await;
    let handler = ThreadSummaryHandler::new(Arc::clone(&f.svc));
    let res = handler.handle(&outbox_msg(b"{not json".to_vec(), 0)).await;
    assert!(matches!(res, MessageResult::Reject(_)), "{res:?}");
    assert!(f.llm.requests().is_empty());
}

#[tokio::test]
async fn handler_maps_run_results() {
    let mut f = fx().await;
    let m = f.conversation("m", 3, 1).await;
    let handler = ThreadSummaryHandler::new(Arc::clone(&f.svc));
    let body = serde_json::to_vec(&f.payload(None, &m[1])).unwrap();

    f.llm.push_ok("", None);
    let retry = handler.handle(&outbox_msg(body.clone(), 0)).await;
    assert!(matches!(retry, MessageResult::Retry), "{retry:?}");
    f.llm.push_ok("", None);
    let reject = handler.handle(&outbox_msg(body.clone(), 2)).await;
    assert!(matches!(reject, MessageResult::Reject(_)), "{reject:?}");

    let ok = handler.handle(&outbox_msg(body, 1)).await;
    assert!(matches!(ok, MessageResult::Ok), "{ok:?}");
    assert!(f.summary_row().await.is_some());
    f.usage_event().await;
}

// ── Pure functions ───────────────────────────────────────────────────────────

#[test]
fn parses_summary_block_and_collapses_blank_lines() {
    let raw = "<analysis>\nstep 1\n\nstep 2\n</analysis>\n\n<summary>\n1. Purpose: tea\n\n\n\n2. Key: green\n   \n\n3. Open: none\n</summary>\n";
    assert_eq!(
        parse_summary(raw),
        "1. Purpose: tea\n\n2. Key: green\n\n3. Open: none"
    );
}

#[test]
fn parse_without_summary_block_keeps_remaining_text() {
    assert_eq!(
        parse_summary("<analysis>x</analysis>\nPlain summary.\n\n\nMore."),
        "Plain summary.\n\nMore."
    );
}

#[test]
fn parse_with_leftover_markup_is_empty() {
    assert_eq!(parse_summary("<analysis>unterminated thinking"), "");
    assert_eq!(parse_summary("<summary>unterminated"), "");
    assert_eq!(
        parse_summary("<analysis>a</analysis><summary>  \n </summary>"),
        ""
    );
    assert_eq!(parse_summary("   \n"), "");
}

#[test]
fn prompt_contains_existing_summary_block_and_entries() {
    let entries = vec![
        (MessageRole::User, "Hello there".to_owned()),
        (MessageRole::System, "hidden system text".to_owned()),
        (MessageRole::Assistant, "abcdefghij".to_owned()),
    ];
    let prompt = build_user_prompt(Some("Old summary."), &entries, 5);
    let expected = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\n\
IMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.\n\n\
<existing_summary>\nOld summary.\n</existing_summary>\n\n\
New messages to incorporate:\n\n\
User: Hello...\n\n\
Assistant: abcde...\n\n";
    assert!(
        prompt.starts_with(expected),
        "prompt start mismatch:\n{prompt}"
    );
    assert!(!prompt.contains("hidden system text"));
    assert!(prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."));
    assert!(
        prompt.contains(
            "Before providing your final summary, wrap your analysis in <analysis> tags."
        )
    );
    assert!(prompt.contains("5. Current Topic: What was being discussed most recently, with enough detail to continue naturally"));
}

#[test]
fn prompt_without_summary_starts_with_plain_instruction() {
    let entries = vec![
        (MessageRole::User, "h\u{e9}llo w\u{f6}rld".to_owned()),
        (MessageRole::Assistant, "short".to_owned()),
    ];
    // Limit counts characters, not bytes; 0 means no truncation.
    let prompt = build_user_prompt(None, &entries, 4);
    assert!(
        prompt.starts_with(
            "Summarize the following conversation:\n\nUser: h\u{e9}ll...\n\nAssistant: shor...\n\n"
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("<existing_summary>"));
    let unlimited = build_user_prompt(None, &entries, 0);
    assert!(unlimited.contains("User: h\u{e9}llo w\u{f6}rld\n\nAssistant: short\n\n"));
}

#[test]
fn token_estimate_rule() {
    assert_eq!(token_estimate(Some(&usage(1, 40, 10)), "abc"), 30);
    // Not positive: ceil(bytes / 4).
    assert_eq!(token_estimate(Some(&usage(1, 10, 10)), "abcde"), 2);
    assert_eq!(token_estimate(Some(&usage(1, 0, 0)), "abcd"), 1);
    assert_eq!(token_estimate(None, "abcdefghi"), 3);
}
