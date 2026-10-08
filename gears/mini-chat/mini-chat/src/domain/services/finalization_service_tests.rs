#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    PolicySnapshot, PublishError, QuotaPolicyDecision, TierLimits, UsageEvent, UsageTokens,
    UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::Value;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::sync::mpsc::UnboundedReceiver;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
use uuid::Uuid;

use super::{FinalizationService, FinalizeInput, SummaryCandidate, TerminalOutcome, ToolCounts};
use crate::config::MiniChatConfig;
use crate::domain::enums::TurnState;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyProvider;
use crate::domain::services::quota_service::{PeriodStarts, QuotaService, ReserveRequest};
use crate::domain::time::{db_now, db_ts};
use crate::infra::db::entities::{chat, chat_turn, message, quota_usage, thread_summary};
use crate::infra::outbox::{AUDIT_PAYLOAD_TYPE, THREAD_SUMMARY_PAYLOAD_TYPE, USAGE_PAYLOAD_TYPE};
use crate::test_support::{
    FakeAuthz, FakePolicy, catalog_entry, ctx_for, insert_message, insert_turn, seed_chat,
    seed_turn, snapshot, test_file_db, test_outbox,
};

// ── Fixture numbers ──────────────────────────────────────────────────────────
//
// Model `p` (premium): in 1_500_000, out 3_000_000 micro-credits per 1M tokens
// => credits(i, o) = ceil(i * 1.5) + ceil(o * 3).
// reserve_tokens = 3000 (1000 estimated input + 2000 max output), floor 100.

const MODEL: &str = "p";
const RESERVE_TOKENS: i64 = 3000;
const MAX_OUTPUT: i64 = 2000;
const FLOOR: i64 = 100;
/// credits(1000, 2000) = 1500 + 6000.
const RESERVED_CREDITS: i64 = 7500;
/// Estimated: credits(1000, 100) = 1500 + 300.
const ESTIMATED_CREDITS: i64 = 1800;

fn limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 1_000_000_000,
        limit_monthly_credits_micro: 10_000_000_000,
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    quota: Arc<QuotaService>,
    svc: FinalizationService,
    rx: UnboundedReceiver<(String, Value)>,
    tenant: Uuid,
    user: Uuid,
    chat: chat::Model,
}

async fn fx() -> Fx {
    fx_with(None).await
}

/// `svc_policy` replaces the policy of the finalization service only (quota
/// keeps the healthy fake).
async fn fx_with(svc_policy: Option<Arc<dyn PolicyProvider>>) -> Fx {
    let (dir, raw) = test_file_db().await;
    let (outbox, rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let policy: Arc<dyn PolicyProvider> = Arc::new(FakePolicy::with_limits(
        snapshot(vec![catalog_entry(MODEL, true)]),
        limits(),
        limits(),
    ));
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&db),
        Arc::new(FakeAuthz::default()),
        Arc::clone(&policy),
        Arc::new(MiniChatConfig::default()),
    ));
    let svc = FinalizationService::new(
        Arc::clone(&db),
        svc_policy.unwrap_or(policy),
        Arc::clone(&quota),
        outbox,
    );
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let chat = seed_chat(
        &db,
        &ctx_for(tenant, user),
        None,
        db_ts(OffsetDateTime::now_utc() - TimeDuration::hours(1)),
    )
    .await;
    Fx {
        _dir: dir,
        db,
        quota,
        svc,
        rx,
        tenant,
        user,
        chat,
    }
}

fn msg_am(
    chat: &chat::Model,
    request_id: Uuid,
    role: &str,
    at: OffsetDateTime,
) -> message::ActiveModel {
    message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(Some(request_id)),
        role: Set(role.to_owned()),
        content: Set(format!("{role} text")),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(db_ts(at)),
        deleted_at: Set(None),
    }
}

/// A running turn with its user message, preflight columns and a booked
/// reserve.
struct Turn {
    row: chat_turn::Model,
    user_msg: message::Model,
    periods: PeriodStarts,
}

async fn running_turn(
    f: &Fx,
    started_at: OffsetDateTime,
    last_progress: Option<OffsetDateTime>,
) -> Turn {
    let started_at = db_ts(started_at);
    let request_id = Uuid::new_v4();
    let user_msg = insert_message(&f.db, msg_am(&f.chat, request_id, "user", started_at)).await;
    let row = insert_turn(
        &f.db,
        chat_turn::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(f.tenant),
            chat_id: Set(f.chat.id),
            request_id: Set(request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(f.user)),
            state: Set("running".to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(Some(RESERVE_TOKENS)),
            max_output_tokens_applied: Set(Some(i32::try_from(MAX_OUTPUT).unwrap())),
            reserved_credits_micro: Set(Some(RESERVED_CREDITS)),
            policy_version_applied: Set(Some(1)),
            effective_model: Set(Some(MODEL.to_owned())),
            minimal_generation_floor_applied: Set(Some(i32::try_from(FLOOR).unwrap())),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(started_at),
            last_progress_at: Set(last_progress.map(db_ts)),
            web_search_enabled: Set(false),
            web_search_completed_count: Set(1),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(2),
            completed_at: Set(None),
            updated_at: Set(started_at),
        },
    )
    .await;
    let periods = PeriodStarts::at(started_at);
    let quota = Arc::clone(&f.quota);
    let req = ReserveRequest {
        tenant_id: f.tenant,
        user_id: f.user,
        premium: true,
        reserved_credits_micro: RESERVED_CREDITS,
        periods,
        limits: mini_chat_sdk::UserLimits {
            user_id: f.user,
            policy_version: 1,
            standard: limits(),
            premium: limits(),
        },
    };
    f.db.transaction(move |tx| Box::pin(async move { quota.reserve_in_tx(tx, &req).await }))
        .await
        .unwrap();
    Turn {
        row,
        user_msg,
        periods,
    }
}

fn input(f: &Fx, t: &Turn, outcome: TerminalOutcome) -> FinalizeInput {
    FinalizeInput {
        turn_id: t.row.id,
        chat_id: f.chat.id,
        tenant_id: f.tenant,
        request_id: t.row.request_id,
        requester_user_id: f.user,
        selected_model: "selected".to_owned(),
        effective_model: MODEL.to_owned(),
        policy_version: 1,
        reserve_tokens: RESERVE_TOKENS,
        reserved_credits_micro: RESERVED_CREDITS,
        max_output_tokens_applied: MAX_OUTPUT,
        minimal_generation_floor_applied: FLOOR,
        periods: t.periods,
        premium: true,
        assistant_message_id: Uuid::new_v4(),
        outcome,
        tool_counts: ToolCounts {
            web_search: 1,
            code_interpreter: 0,
            file_search: 2,
        },
        quota_decision: QuotaPolicyDecision {
            decision: "downgrade".to_owned(),
            downgrade_from: Some("selected".to_owned()),
            downgrade_reason: Some("premium_quota_exhausted".to_owned()),
        },
        latency_ms: 1234,
        summary_candidate: None,
    }
}

fn usage(input: i64, output: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 7,
        cache_write_input_tokens: 0,
        reasoning_tokens: 3,
    }
}

fn completed(text: &str, u: Option<UsageTokens>) -> TerminalOutcome {
    TerminalOutcome::Completed {
        text: text.to_owned(),
        usage: u,
        response_id: Some("resp_abc".to_owned()),
        incomplete_reason: None,
    }
}

async fn turn_row(f: &Fx, id: Uuid) -> chat_turn::Model {
    let conn = f.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

async fn assistant_messages(f: &Fx, request_id: Uuid) -> Vec<message::Model> {
    let conn = f.db.conn().unwrap();
    message::Entity::find()
        .filter(message::Column::RequestId.eq(request_id))
        .filter(message::Column::Role.eq("assistant"))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

/// `(bucket, period_type) -> (spent, reserved)` of the user's quota rows.
async fn quota_rows(f: &Fx) -> Vec<(String, String, i64, i64)> {
    let conn = f.db.conn().unwrap();
    let mut rows: Vec<_> = quota_usage::Entity::find()
        .filter(quota_usage::Column::UserId.eq(f.user))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r.bucket,
                r.period_type,
                r.spent_credits_micro,
                r.reserved_credits_micro,
            )
        })
        .collect();
    rows.sort();
    rows
}

/// Asserts all four bucket rows were settled to `spent` with no reserve left.
async fn assert_settled(f: &Fx, spent: i64) {
    let rows = quota_rows(f).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for (bucket, period, s, r) in rows {
        assert_eq!((s, r), (spent, 0), "{bucket}/{period}");
    }
}

/// Waits for exactly `n` delivered outbox messages, then makes sure no more
/// arrive.
async fn delivered(f: &mut Fx, n: usize) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while out.len() < n {
        let msg = tokio::time::timeout(Duration::from_secs(20), f.rx.recv())
            .await
            .expect("outbox message delivered in time")
            .unwrap();
        out.push(msg);
    }
    assert_no_more(f).await;
    out
}

async fn assert_no_more(f: &mut Fx) {
    if let Ok(Some(extra)) = tokio::time::timeout(Duration::from_millis(1500), f.rx.recv()).await {
        panic!("unexpected outbox message {extra:?}");
    }
}

fn one_of(msgs: &[(String, Value)], payload_type: &str) -> Value {
    let found: Vec<_> = msgs.iter().filter(|(t, _)| t == payload_type).collect();
    assert_eq!(found.len(), 1, "{payload_type} in {msgs:?}");
    found[0].1.clone()
}

fn dedupe(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant.as_simple(),
        turn.as_simple(),
        request.as_simple()
    )
}

// ── Stream finalization ──────────────────────────────────────────────────────

#[tokio::test]
async fn completed_persists_message_settles_and_enqueues_once() {
    let mut f = fx().await;
    let t = running_turn(
        &f,
        OffsetDateTime::now_utc() - TimeDuration::seconds(5),
        None,
    )
    .await;
    let inp = input(&f, &t, completed("Hello there", Some(usage(400, 200))));
    let msg_id = inp.assistant_message_id;

    let res = f.svc.finalize(inp).await.unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Completed);
    assert!(res.quota_warnings.is_some());
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "completed");
    assert_eq!(row.assistant_message_id, Some(msg_id));
    assert_eq!(row.error_code, None);
    assert_eq!(row.provider_response_id.as_deref(), Some("resp_abc"));
    assert!(row.completed_at.is_some());

    let msgs = assistant_messages(&f, t.row.request_id).await;
    assert_eq!(msgs.len(), 1);
    let msg = &msgs[0];
    assert_eq!(msg.id, msg_id);
    assert_eq!(msg.content, "Hello there");
    assert_eq!(msg.model.as_deref(), Some(MODEL));
    assert_eq!((msg.input_tokens, msg.output_tokens), (400, 200));
    assert_eq!((msg.cache_read_input_tokens, msg.reasoning_tokens), (7, 3));
    assert_eq!(msg.provider_response_id.as_deref(), Some("resp_abc"));

    // credits(400, 200) = 600 + 600
    assert_settled(&f, 1200).await;

    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "completed");
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["terminal_state"], "completed");
    assert_eq!(u["actual_credits_micro"], 1200);
    assert_eq!(u["requester_type"], "user");
    assert_eq!(u["selected_model"], "selected");
    assert_eq!(u["effective_model"], MODEL);
    assert_eq!(u["policy_version_applied"], 1);
    assert_eq!(u["web_search_calls"], 1);
    assert_eq!(u["file_search_calls"], 2);
    assert_eq!(u["usage"]["input_tokens"], 400);
    assert_eq!(u["user_id"], f.user.to_string());
    assert_eq!(u["turn_id"], t.row.id.to_string());
    assert_eq!(
        u["dedupe_key"],
        dedupe(f.tenant, t.row.id, t.row.request_id)
    );
    let a = one_of(&out, AUDIT_PAYLOAD_TYPE);
    assert_eq!(a["kind"], "turn");
    assert_eq!(a["event_type"], "turn_completed");
    assert_eq!(a["terminal_state"], "completed");
    assert_eq!(a["error_code"], Value::Null);
    assert_eq!(a["latency_ms"], 1234);
    assert_eq!(a["policy_decisions"]["quota"]["decision"], "downgrade");
    assert_eq!(a["tool_calls"]["web_search_calls"], 1);
    assert_eq!(a["prompt"], Value::Null);
    assert_eq!(a["attachments"], serde_json::json!([]));
}

#[tokio::test]
async fn second_finalize_is_noop() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let first = f
        .svc
        .finalize(input(&f, &t, completed("one", Some(usage(400, 200)))))
        .await
        .unwrap();
    assert!(first.won);
    delivered(&mut f, 2).await;
    let quota_before = quota_rows(&f).await;

    let second = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Cancelled {
                partial_text: "two".to_owned(),
            },
        ))
        .await
        .unwrap();

    assert!(!second.won);
    assert_eq!(second.state, TurnState::Completed);
    assert_eq!(second.quota_warnings, None);
    assert_eq!(quota_rows(&f).await, quota_before);
    assert_eq!(assistant_messages(&f, t.row.request_id).await.len(), 1);
    assert_eq!(turn_row(&f, t.row.id).await.state, "completed");
    assert_no_more(&mut f).await;
}

#[tokio::test]
async fn failed_without_usage_settles_estimated() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Failed {
                error_code: "provider_error".to_owned(),
                error_detail: Some("upstream said no".to_owned()),
                // zero usage counts as unknown
                usage: Some(UsageTokens::default()),
                partial_text: "partial".to_owned(),
            },
        ))
        .await
        .unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Failed);
    assert_eq!(res.quota_warnings, None);
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("provider_error"));
    assert_eq!(row.error_detail.as_deref(), Some("upstream said no"));
    assert_eq!(row.assistant_message_id, None);
    assert!(assistant_messages(&f, t.row.request_id).await.is_empty());
    assert_settled(&f, ESTIMATED_CREDITS).await;

    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "failed");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["actual_credits_micro"], ESTIMATED_CREDITS);
    let a = one_of(&out, AUDIT_PAYLOAD_TYPE);
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "provider_error");
}

#[tokio::test]
async fn failed_with_usage_settles_actual() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Failed {
                error_code: "provider_error".to_owned(),
                error_detail: None,
                usage: Some(usage(500, 100)),
                partial_text: String::new(),
            },
        ))
        .await
        .unwrap();

    assert!(res.won);
    // credits(500, 100) = 750 + 300
    assert_settled(&f, 1050).await;
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "failed");
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["usage"]["output_tokens"], 100);
    assert_eq!(u["actual_credits_micro"], 1050);
}

#[tokio::test]
async fn pre_provider_failure_releases_reserve() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    f.svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Failed {
                error_code: "context_length_exceeded".to_owned(),
                error_detail: None,
                usage: None,
                partial_text: String::new(),
            },
        ))
        .await
        .unwrap();

    assert_settled(&f, 0).await;
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["settlement_method"], "released");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["usage"]["input_tokens"], 0);
    assert_eq!(u["usage"]["output_tokens"], 0);
}

#[tokio::test]
async fn cancelled_persists_partial_and_aborted_estimated() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let inp = input(
        &f,
        &t,
        TerminalOutcome::Cancelled {
            partial_text: "half an ans".to_owned(),
        },
    );
    let msg_id = inp.assistant_message_id;
    let res = f.svc.finalize(inp).await.unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Cancelled);
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(row.error_code, None);
    assert_eq!(row.assistant_message_id, Some(msg_id));
    let msgs = assistant_messages(&f, t.row.request_id).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "half an ans");
    assert_eq!((msgs[0].input_tokens, msgs[0].output_tokens), (0, 0));
    assert_settled(&f, ESTIMATED_CREDITS).await;

    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "cancelled");
    assert_eq!(u["usage"], Value::Null);
    let a = one_of(&out, AUDIT_PAYLOAD_TYPE);
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["terminal_state"], "cancelled");
}

#[tokio::test]
async fn cancelled_empty_text_has_no_message() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Cancelled {
                partial_text: String::new(),
            },
        ))
        .await
        .unwrap();

    assert!(res.won);
    assert_eq!(turn_row(&f, t.row.id).await.assistant_message_id, None);
    assert!(assistant_messages(&f, t.row.request_id).await.is_empty());
    assert_settled(&f, ESTIMATED_CREDITS).await;
    delivered(&mut f, 2).await;
}

#[tokio::test]
async fn cancelled_message_failure_still_cancels_without_message() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    // An assistant message with the turn's request id already exists: the
    // (chat_id, request_id, role) unique index rejects the partial message.
    let clash = insert_message(
        &f.db,
        msg_am(
            &f.chat,
            t.row.request_id,
            "assistant",
            OffsetDateTime::now_utc(),
        ),
    )
    .await;

    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Cancelled {
                partial_text: "partial".to_owned(),
            },
        ))
        .await
        .unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Cancelled);
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(row.assistant_message_id, None);
    let msgs = assistant_messages(&f, t.row.request_id).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].id, clash.id);
    assert_settled(&f, ESTIMATED_CREDITS).await;
    let out = delivered(&mut f, 2).await;
    assert_eq!(
        one_of(&out, USAGE_PAYLOAD_TYPE)["billing_outcome"],
        "aborted"
    );
}

#[tokio::test]
async fn completed_message_failure_finalizes_failed_message_persistence() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    insert_message(
        &f.db,
        msg_am(
            &f.chat,
            t.row.request_id,
            "assistant",
            OffsetDateTime::now_utc(),
        ),
    )
    .await;

    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            completed("full answer", Some(usage(400, 200))),
        ))
        .await
        .unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Failed);
    assert_eq!(res.quota_warnings, None);
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(
        row.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(row.assistant_message_id, None);
    // usage was reported: post-provider failure settles actual
    assert_settled(&f, 1200).await;
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "failed");
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["terminal_state"], "failed");
    let a = one_of(&out, AUDIT_PAYLOAD_TYPE);
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "message_persistence_failed");
}

#[tokio::test]
async fn incomplete_is_completed_without_error_code() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let res = f
        .svc
        .finalize(input(
            &f,
            &t,
            TerminalOutcome::Completed {
                text: "truncated".to_owned(),
                usage: Some(usage(400, 2000)),
                response_id: None,
                incomplete_reason: Some("max_tokens".to_owned()),
            },
        ))
        .await
        .unwrap();

    assert!(res.won);
    assert_eq!(res.state, TurnState::Completed);
    let row = turn_row(&f, t.row.id).await;
    assert_eq!(row.state, "completed");
    assert_eq!(row.error_code, None);
    assert_eq!(row.error_detail, None);
    assert_eq!(assistant_messages(&f, t.row.request_id).await.len(), 1);
    // credits(400, 2000) = 600 + 6000
    assert_settled(&f, 6600).await;
    let out = delivered(&mut f, 2).await;
    assert_eq!(
        one_of(&out, USAGE_PAYLOAD_TYPE)["billing_outcome"],
        "completed"
    );
    assert_eq!(
        one_of(&out, AUDIT_PAYLOAD_TYPE)["event_type"],
        "turn_completed"
    );
}

#[tokio::test]
async fn missing_model_in_snapshot_fails_and_turn_stays_running() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    let mut inp = input(&f, &t, completed("x", Some(usage(1, 1))));
    inp.effective_model = "gone".to_owned();

    let err = f.svc.finalize(inp).await.unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    assert_eq!(turn_row(&f, t.row.id).await.state, "running");
    assert!(assistant_messages(&f, t.row.request_id).await.is_empty());
    let rows = quota_rows(&f).await;
    assert!(rows.iter().all(|r| r.3 == RESERVED_CREDITS), "{rows:?}");
    assert_no_more(&mut f).await;
}

#[tokio::test]
async fn finalize_unstarted_is_plain_cas() {
    let mut f = fx().await;
    let turn = seed_turn(&f.db, &f.chat, Uuid::new_v4(), "running", db_now()).await;

    f.svc
        .finalize_unstarted(f.tenant, turn.id, "turn_setup_failed")
        .await
        .unwrap();

    let row = turn_row(&f, turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("turn_setup_failed"));
    assert!(row.completed_at.is_some());
    assert!(quota_rows(&f).await.is_empty());
    assert_no_more(&mut f).await;

    // a second call does not touch the terminal row
    f.svc
        .finalize_unstarted(f.tenant, turn.id, "quota_exceeded")
        .await
        .unwrap();
    assert_eq!(
        turn_row(&f, turn.id).await.error_code.as_deref(),
        Some("turn_setup_failed")
    );
}

// ── Orphan watchdog ──────────────────────────────────────────────────────────

#[tokio::test]
async fn orphan_finalize_requires_stale_predicate() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    let cutoff = now - TimeDuration::minutes(5);
    // started long ago but made progress after the cutoff: not an orphan
    let fresh = running_turn(
        &f,
        now - TimeDuration::hours(1),
        Some(now - TimeDuration::minutes(1)),
    )
    .await;

    assert!(!f.svc.finalize_orphan(&fresh.row, cutoff).await.unwrap());
    assert_eq!(turn_row(&f, fresh.row.id).await.state, "running");
    assert_no_more(&mut f).await;

    // finalize the fresh turn normally so the chat's running slot is free
    f.svc
        .finalize(input(
            &f,
            &fresh,
            TerminalOutcome::Cancelled {
                partial_text: String::new(),
            },
        ))
        .await
        .unwrap();
    delivered(&mut f, 2).await;
    let spent_before: i64 = quota_rows(&f).await[0].2;

    let stale = running_turn(
        &f,
        now - TimeDuration::hours(1),
        Some(now - TimeDuration::minutes(10)),
    )
    .await;
    assert!(f.svc.finalize_orphan(&stale.row, cutoff).await.unwrap());

    let row = turn_row(&f, stale.row.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert!(row.completed_at.is_some());
    let rows = quota_rows(&f).await;
    for (bucket, period, spent, reserved) in rows {
        assert_eq!(
            (spent, reserved),
            (spent_before + ESTIMATED_CREDITS, 0),
            "{bucket}/{period}"
        );
    }
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["selected_model"], MODEL);
    assert_eq!(u["effective_model"], MODEL);
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["actual_credits_micro"], ESTIMATED_CREDITS);
    assert_eq!(u["web_search_calls"], 1);
    assert_eq!(u["file_search_calls"], 2);
    assert_eq!(
        u["dedupe_key"],
        dedupe(f.tenant, stale.row.id, stale.row.request_id)
    );
    let a = one_of(&out, AUDIT_PAYLOAD_TYPE);
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "orphan_timeout");
    assert_eq!(a["policy_decisions"]["quota"]["decision"], "unknown");

    // a repeated scan finds nothing to do
    assert!(!f.svc.finalize_orphan(&stale.row, cutoff).await.unwrap());
    assert_no_more(&mut f).await;
}

#[tokio::test]
async fn orphan_without_reserve_skips_settlement_but_enqueues_usage() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    // retry/edit turn left running before its preflight columns were written
    let turn = seed_turn(
        &f.db,
        &f.chat,
        Uuid::new_v4(),
        "running",
        db_ts(now - TimeDuration::hours(1)),
    )
    .await;

    assert!(
        f.svc
            .finalize_orphan(&turn, now - TimeDuration::minutes(5))
            .await
            .unwrap()
    );

    let row = turn_row(&f, turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert!(quota_rows(&f).await.is_empty());
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["effective_model"], "");
    assert_eq!(u["selected_model"], "");
    assert_eq!(u["policy_version_applied"], 0);
    assert_eq!(
        one_of(&out, AUDIT_PAYLOAD_TYPE)["event_type"],
        "turn_failed"
    );
}

#[tokio::test]
async fn orphan_with_model_missing_from_snapshot_still_finalizes() {
    // DESIGN section 5.8: a turn must never stay running indefinitely. The
    // effective model vanished from the snapshot, so the estimate cannot be
    // priced: the turn is finalized anyway with settlement skipped.
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    let t = running_turn(&f, now - TimeDuration::hours(1), None).await;
    let mut row = t.row.clone();
    row.effective_model = Some("gone".to_owned());

    assert!(
        f.svc
            .finalize_orphan(&row, now - TimeDuration::minutes(5))
            .await
            .unwrap()
    );

    let turn = turn_row(&f, t.row.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    // nothing settled: the reserve is left as booked, nothing spent
    let rows = quota_rows(&f).await;
    assert!(
        rows.iter().all(|r| r.2 == 0 && r.3 == RESERVED_CREDITS),
        "{rows:?}"
    );
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["effective_model"], "gone");
    assert_eq!(u["policy_version_applied"], 1);
    assert_eq!(
        one_of(&out, AUDIT_PAYLOAD_TYPE)["error_code"],
        "orphan_timeout"
    );
}

#[tokio::test]
async fn orphan_with_dropped_policy_version_still_finalizes() {
    // The plugin no longer has the turn's policy version (NotFound): that is
    // permanent, so the orphan is finalized like the missing-model case
    // (CAS, no settlement, usage event with 0 credits) rather than retried
    // forever.
    let policy: Arc<dyn PolicyProvider> = Arc::new(FailingSnapshot(
        DomainError::PolicySnapshotGone("version 1 dropped".to_owned()),
    ));
    let mut f = fx_with(Some(policy)).await;
    let now = OffsetDateTime::now_utc();
    let t = running_turn(&f, now - TimeDuration::hours(1), None).await;

    assert!(
        f.svc
            .finalize_orphan(&t.row, now - TimeDuration::minutes(5))
            .await
            .unwrap()
    );

    let turn = turn_row(&f, t.row.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    let rows = quota_rows(&f).await;
    assert!(
        rows.iter().all(|r| r.2 == 0 && r.3 == RESERVED_CREDITS),
        "{rows:?}"
    );
    let out = delivered(&mut f, 2).await;
    let u = one_of(&out, USAGE_PAYLOAD_TYPE);
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["policy_version_applied"], 1);
    assert_eq!(
        one_of(&out, AUDIT_PAYLOAD_TYPE)["error_code"],
        "orphan_timeout"
    );
}

#[tokio::test]
async fn orphan_policy_failure_leaves_turn_running() {
    let (dir, raw) = test_file_db().await;
    let (outbox, _rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let failing: Arc<dyn PolicyProvider> = Arc::new(FailingSnapshot(DomainError::Internal(
        "policy plugin unavailable".to_owned(),
    )));
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&db),
        Arc::new(FakeAuthz::default()),
        Arc::clone(&failing),
        Arc::new(MiniChatConfig::default()),
    ));
    let svc = FinalizationService::new(Arc::clone(&db), failing, quota, outbox);
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let now = OffsetDateTime::now_utc();
    let chat = seed_chat(&db, &ctx_for(tenant, user), None, db_ts(now)).await;
    let mut turn = seed_turn(
        &db,
        &chat,
        Uuid::new_v4(),
        "running",
        db_ts(now - TimeDuration::hours(1)),
    )
    .await;
    turn.reserve_tokens = Some(RESERVE_TOKENS);
    turn.reserved_credits_micro = Some(RESERVED_CREDITS);
    turn.max_output_tokens_applied = Some(2000);
    turn.minimal_generation_floor_applied = Some(100);
    turn.policy_version_applied = Some(1);
    turn.effective_model = Some(MODEL.to_owned());

    let err = svc
        .finalize_orphan(&turn, now - TimeDuration::minutes(5))
        .await
        .unwrap_err();

    // a transient plugin failure is retried by the next scan
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    let conn = db.conn().unwrap();
    let row = chat_turn::Entity::find_by_id(turn.id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "running");
    drop(dir);
}

/// Policy whose snapshot lookup fails (plugin unavailable); nothing else is
/// expected to be called.
struct FailingSnapshot(DomainError);

#[async_trait::async_trait]
impl PolicyProvider for FailingSnapshot {
    async fn current(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        panic!("FailingSnapshot::current called");
    }

    async fn snapshot(
        &self,
        _user_id: Uuid,
        _version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        Err(self.0.clone())
    }

    async fn user_limits(&self, _user_id: Uuid, _version: u64) -> Result<UserLimits, DomainError> {
        panic!("FailingSnapshot::user_limits called");
    }

    async fn publish_usage(&self, _ev: UsageEvent) -> Result<(), PublishError> {
        panic!("FailingSnapshot::publish_usage called");
    }
}

// ── Thread summary trigger ───────────────────────────────────────────────────

/// A completed earlier turn: user + assistant messages at `at` and `at + 1s`.
async fn prior_turn(f: &Fx, at: OffsetDateTime) -> (message::Model, message::Model) {
    let rid = Uuid::new_v4();
    let u = insert_message(&f.db, msg_am(&f.chat, rid, "user", at)).await;
    let a = insert_message(
        &f.db,
        msg_am(&f.chat, rid, "assistant", at + TimeDuration::seconds(1)),
    )
    .await;
    (u, a)
}

fn with_trigger(mut inp: FinalizeInput) -> FinalizeInput {
    inp.summary_candidate = Some(SummaryCandidate { trigger: true });
    inp
}

#[tokio::test]
async fn summary_enqueued_when_trigger_and_prior_message() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    let (_u1, a1) = prior_turn(&f, now - TimeDuration::minutes(10)).await;
    let t = running_turn(&f, now - TimeDuration::minutes(1), None).await;
    assert!(t.user_msg.created_at > a1.created_at);

    f.svc
        .finalize(with_trigger(input(
            &f,
            &t,
            completed("ok", Some(usage(4, 2))),
        )))
        .await
        .unwrap();

    let out = delivered(&mut f, 3).await;
    let s = one_of(&out, THREAD_SUMMARY_PAYLOAD_TYPE);
    assert_eq!(s["tenant_id"], f.tenant.to_string());
    assert_eq!(s["chat_id"], f.chat.id.to_string());
    assert_eq!(s["system_task_type"], "thread_summary_update");
    assert_eq!(s["frozen_target_message_id"], a1.id.to_string());
    assert_eq!(s["base_frontier_message_id"], Value::Null);
    assert_eq!(s["base_frontier_created_at"], Value::Null);
    let sys_id: Uuid = s["system_request_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(sys_id.get_version_num(), 4);
}

#[tokio::test]
async fn summary_uses_frontier_and_skips_covered_target() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    let (_u1, a1) = prior_turn(&f, now - TimeDuration::minutes(20)).await;
    let conn = f.db.conn().unwrap();
    let summary_at = db_now();
    secure_insert::<thread_summary::Entity>(
        thread_summary::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(f.tenant),
            chat_id: Set(f.chat.id),
            summary_text: Set("so far".to_owned()),
            summarized_up_to_created_at: Set(a1.created_at),
            summarized_up_to_message_id: Set(a1.id),
            token_estimate: Set(3),
            created_at: Set(summary_at),
            updated_at: Set(summary_at),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();

    // target (a1) == frontier: nothing new to summarize
    let t1 = running_turn(&f, now - TimeDuration::minutes(15), None).await;
    f.svc
        .finalize(with_trigger(input(&f, &t1, completed("ok", None))))
        .await
        .unwrap();
    let out = delivered(&mut f, 2).await;
    assert!(out.iter().all(|(t, _)| t != THREAD_SUMMARY_PAYLOAD_TYPE));

    // next turn (after t1's assistant message, written at finalization
    // time): t1's assistant message is the new target, a1 the base
    let t2 = running_turn(
        &f,
        OffsetDateTime::now_utc() + TimeDuration::minutes(1),
        None,
    )
    .await;
    f.svc
        .finalize(with_trigger(input(&f, &t2, completed("ok", None))))
        .await
        .unwrap();
    let out = delivered(&mut f, 3).await;
    let s = one_of(&out, THREAD_SUMMARY_PAYLOAD_TYPE);
    let t1_msg = &assistant_messages(&f, t1.row.request_id).await[0];
    assert_eq!(s["frozen_target_message_id"], t1_msg.id.to_string());
    assert_eq!(s["base_frontier_message_id"], a1.id.to_string());
}

#[tokio::test]
async fn no_summary_for_first_turn() {
    let mut f = fx().await;
    let t = running_turn(&f, OffsetDateTime::now_utc(), None).await;
    f.svc
        .finalize(with_trigger(input(&f, &t, completed("ok", None))))
        .await
        .unwrap();
    let out = delivered(&mut f, 2).await;
    assert!(out.iter().all(|(t, _)| t != THREAD_SUMMARY_PAYLOAD_TYPE));
}

#[tokio::test]
async fn no_summary_for_failed_turn() {
    let mut f = fx().await;
    let now = OffsetDateTime::now_utc();
    prior_turn(&f, now - TimeDuration::minutes(10)).await;
    let t = running_turn(&f, now, None).await;
    f.svc
        .finalize(with_trigger(input(
            &f,
            &t,
            TerminalOutcome::Failed {
                error_code: "provider_error".to_owned(),
                error_detail: None,
                usage: None,
                partial_text: String::new(),
            },
        )))
        .await
        .unwrap();
    let out = delivered(&mut f, 2).await;
    assert!(out.iter().all(|(t, _)| t != THREAD_SUMMARY_PAYLOAD_TYPE));
}
