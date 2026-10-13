#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use sea_orm::EntityTrait;
use serde_json::{Value, json};
use toolkit_db::Db;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

use super::*;
use crate::domain::clock::now_utc;
use crate::domain::estimation::period_starts;
use crate::domain::model::{
    BillingOutcome, Bucket, DowngradeReason, PeriodType, QuotaDecision, SettlementMethod,
    TurnState, error_codes,
};
use crate::domain::services::QuotaTier;
use crate::infra::db::entities::{chat_turn, message, quota_usage};
use crate::infra::db::repos::QuotaRepo;
use crate::infra::db::repos::chat::{ChatRepo, NewChat};
use crate::infra::db::repos::turn::{NewTurn, PreflightFields, TurnCounters, TurnRepo};
use crate::infra::llm::types::LlmUsage;
use crate::infra::outbox::QueueKind;
use crate::infra::outbox::payloads::{THREAD_SUMMARY_TASK_TYPE, ThreadSummaryPayload};
use crate::testing::seed::{NewMessage, insert_message_with, insert_quota_row};
use crate::testing::{TestApp, TestUser};

const USER: TestUser = TestUser::A1;
/// `gpt-premium`: input multiplier 1.0, output 3.0 (credits = in + 3 * out).
const MODEL: &str = "gpt-premium";
const RESERVE_TOKENS: i64 = 1000;
const MAX_OUT: i32 = 400;
const FLOOR: i32 = 50;
/// `credits(600, 400)`.
const RESERVED: i64 = 600 + 3 * 400;
/// `credits(1000 - 400, 50)`.
const ESTIMATED: i64 = 600 + 3 * 50;

struct Fixture {
    app: TestApp,
    turn: chat_turn::Model,
}

async fn fixture_with_model(effective_model: &str) -> Fixture {
    let app = TestApp::builder().build().await;
    let conn = app.db.conn().unwrap();
    let now = now_utc();
    let chat_id = Uuid::new_v4();
    ChatRepo::insert(
        &conn,
        &AccessScope::allow_all(),
        NewChat {
            id: chat_id,
            tenant_id: USER.tenant_id,
            user_id: USER.user_id,
            model: MODEL.to_owned(),
            title: None,
            now,
        },
    )
    .await
    .unwrap();
    let turn = TurnRepo::insert_running(
        &conn,
        NewTurn {
            id: Uuid::new_v4(),
            tenant_id: USER.tenant_id,
            chat_id,
            request_id: Uuid::new_v4(),
            requester_user_id: USER.user_id,
            web_search_enabled: false,
            preflight: Some(PreflightFields {
                reserve_tokens: RESERVE_TOKENS,
                max_output_tokens_applied: MAX_OUT,
                reserved_credits_micro: RESERVED,
                policy_version_applied: 1,
                effective_model: effective_model.to_owned(),
                minimal_generation_floor_applied: FLOOR,
            }),
            now,
        },
    )
    .await
    .unwrap();
    let (daily, monthly) = period_starts(now);
    for bucket in ["total", "tier:premium"] {
        for (period, start) in [(PeriodType::Daily, daily), (PeriodType::Monthly, monthly)] {
            insert_quota_row(
                &app.db,
                USER.tenant_id,
                USER.user_id,
                bucket,
                period,
                start,
                0,
                RESERVED,
            )
            .await;
        }
    }
    Fixture { app, turn }
}

async fn fixture() -> Fixture {
    fixture_with_model(MODEL).await
}

fn input(
    f: &Fixture,
    terminal: TerminalKind,
    text: &str,
    usage: Option<LlmUsage>,
) -> FinalizeInput {
    let (daily_start, monthly_start) = period_starts(now_utc());
    FinalizeInput {
        turn: f.turn.clone(),
        chat_model: MODEL.to_owned(),
        user_id: USER.user_id,
        terminal,
        text: text.to_owned(),
        usage,
        provider_response_id: Some("resp_abc".to_owned()),
        assistant_message_id: Uuid::new_v4(),
        counters: TurnCounters {
            web_search: 1,
            code_interpreter: 0,
            file_search: 2,
        },
        file_search_calls: 3,
        daily_start,
        monthly_start,
        decision: Some((QuotaDecision::Allow, None)),
        latency_ms: 1234,
        thread_summary: None,
    }
}

fn completed() -> TerminalKind {
    TerminalKind::Completed {
        incomplete_reason: None,
    }
}

fn failed(code: &str) -> TerminalKind {
    TerminalKind::Failed {
        code: code.to_owned(),
        detail: "upstream said no".to_owned(),
    }
}

fn usage(input_tokens: i64, output_tokens: i64) -> LlmUsage {
    LlmUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens: 7,
        cache_write_input_tokens: 0,
        reasoning_tokens: 3,
    }
}

async fn turn_row(db: &Db, id: Uuid) -> chat_turn::Model {
    chat_turn::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&db.conn().unwrap())
        .await
        .unwrap()
        .unwrap()
}

async fn message_row(db: &Db, id: Uuid) -> Option<message::Model> {
    message::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&db.conn().unwrap())
        .await
        .unwrap()
}

async fn quota_rows(db: &Db) -> Vec<quota_usage::Model> {
    let (daily, monthly) = period_starts(now_utc());
    QuotaRepo::rows_for_periods(
        &db.conn().unwrap(),
        USER.tenant_id,
        USER.user_id,
        daily,
        monthly,
    )
    .await
    .unwrap()
}

/// All rows of `bucket` (daily and monthly).
async fn bucket_rows(db: &Db, bucket: Bucket) -> Vec<quota_usage::Model> {
    let rows: Vec<_> = quota_rows(db)
        .await
        .into_iter()
        .filter(|r| r.bucket == bucket.as_str())
        .collect();
    assert_eq!(rows.len(), 2, "daily + monthly rows of {bucket}");
    rows
}

/// Messages delivered to `queue` once deliveries settled (waits for at least
/// one, then a little longer to catch duplicates).
async fn delivered(app: &TestApp, queue: QueueKind) -> Vec<Value> {
    app.outbox_messages(queue).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    app.outbox_messages_n(queue, 0).await
}

fn dedupe_key(turn: &chat_turn::Model) -> String {
    format!(
        "{}/{}/{}",
        turn.tenant_id.simple(),
        turn.id.simple(),
        turn.request_id.simple()
    )
}

#[test]
fn derive_billing_table() {
    use BillingOutcome as B;
    use SettlementMethod as S;
    let known = usage(10, 0);
    let zero = LlmUsage::default();
    let failed =
        |code: &str, u: Option<&LlmUsage>| derive_billing(TurnState::Failed, Some(code), u);

    // completed: always actual, even without usage.
    assert_eq!(
        derive_billing(TurnState::Completed, None, Some(&known)),
        (B::Completed, S::Actual)
    );
    assert_eq!(
        derive_billing(TurnState::Completed, None, None),
        (B::Completed, S::Actual)
    );
    assert_eq!(
        derive_billing(TurnState::Completed, None, Some(&zero)),
        (B::Completed, S::Actual)
    );

    // post-provider failures: actual iff usage has a non-zero input/output count.
    for code in [
        error_codes::PROVIDER_ERROR,
        error_codes::PROVIDER_TIMEOUT,
        error_codes::RATE_LIMITED,
        error_codes::WEB_SEARCH_CALLS_EXCEEDED,
        error_codes::CODE_INTERPRETER_CALLS_EXCEEDED,
        error_codes::AGENTIC_ITERATIONS_EXCEEDED,
        error_codes::UNEXPECTED_TOOL_USE,
        error_codes::MESSAGE_PERSISTENCE_FAILED,
    ] {
        assert_eq!(failed(code, Some(&known)), (B::Failed, S::Actual), "{code}");
        assert_eq!(
            failed(code, Some(&usage(0, 5))),
            (B::Failed, S::Actual),
            "{code}"
        );
        assert_eq!(failed(code, None), (B::Failed, S::Estimated), "{code}");
        assert_eq!(
            failed(code, Some(&zero)),
            (B::Failed, S::Estimated),
            "{code}"
        );
    }
    // Only cache/reasoning counts set: still "usage unknown".
    let cache_only = LlmUsage {
        cache_read_input_tokens: 9,
        reasoning_tokens: 4,
        ..LlmUsage::default()
    };
    assert_eq!(
        failed(error_codes::PROVIDER_ERROR, Some(&cache_only)),
        (B::Failed, S::Estimated)
    );

    // pre-provider failures: released.
    for code in [
        error_codes::CONTEXT_LENGTH_EXCEEDED,
        error_codes::VALIDATION_ERROR,
        error_codes::INPUT_TOO_LONG,
        error_codes::TURN_SETUP_FAILED,
    ] {
        assert_eq!(
            failed(code, Some(&known)),
            (B::Failed, S::Released),
            "{code}"
        );
        assert_eq!(failed(code, None), (B::Failed, S::Released), "{code}");
    }

    // cancelled and orphan timeout: aborted, always estimated.
    assert_eq!(
        derive_billing(TurnState::Cancelled, None, None),
        (B::Aborted, S::Estimated)
    );
    assert_eq!(
        derive_billing(TurnState::Cancelled, None, Some(&known)),
        (B::Aborted, S::Estimated)
    );
    assert_eq!(
        failed(error_codes::ORPHAN_TIMEOUT, None),
        (B::Aborted, S::Estimated)
    );
    assert_eq!(
        failed(error_codes::ORPHAN_TIMEOUT, Some(&known)),
        (B::Aborted, S::Estimated)
    );

    // unknown (or missing) code: failed, estimated.
    assert_eq!(
        failed("something_new", Some(&known)),
        (B::Failed, S::Estimated)
    );
    assert_eq!(
        derive_billing(TurnState::Failed, None, None),
        (B::Failed, S::Estimated)
    );
}

#[tokio::test]
async fn completed_persists_message_settles_actual_and_enqueues_usage_and_audit() {
    let f = fixture().await;
    let mut inp = input(&f, completed(), "Hello there", Some(usage(100, 20)));
    inp.decision = Some((
        QuotaDecision::Downgrade,
        Some(DowngradeReason::PremiumQuotaExhausted),
    ));
    let msg_id = inp.assistant_message_id;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(
        matches!(
            res,
            FinalizeResult::Won {
                state: TurnState::Completed,
                ..
            }
        ),
        "{res:?}"
    );

    let msg = message_row(&f.app.db, msg_id)
        .await
        .expect("assistant message");
    assert_eq!(msg.role, "assistant");
    assert_eq!(msg.content, "Hello there");
    assert_eq!(msg.chat_id, f.turn.chat_id);
    assert_eq!(msg.tenant_id, f.turn.tenant_id);
    assert_eq!(msg.request_id, Some(f.turn.request_id));
    assert_eq!(msg.model.as_deref(), Some(MODEL));
    assert_eq!(msg.provider_response_id.as_deref(), Some("resp_abc"));
    assert_eq!((msg.input_tokens, msg.output_tokens), (100, 20));
    assert_eq!(msg.cache_read_input_tokens, 7);
    assert_eq!(msg.cache_write_input_tokens, 0);
    assert_eq!(msg.reasoning_tokens, 3);
    assert!(msg.deleted_at.is_none());

    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(msg_id));
    assert_eq!(turn.provider_response_id.as_deref(), Some("resp_abc"));
    assert!(turn.error_code.is_none());
    assert!(turn.completed_at.is_some());
    assert_eq!(turn.web_search_completed_count, 1);
    assert_eq!(turn.file_search_completed_count, 2);

    let committed = 100 + 3 * 20;
    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, committed);
        assert_eq!(r.calls, 1);
        assert_eq!((r.input_tokens, r.output_tokens), (100, 20));
        assert_eq!(r.web_search_calls, 1);
    }
    for r in bucket_rows(&f.app.db, Bucket::TierPremium).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, committed);
        assert_eq!(r.calls, 1);
    }

    let usage_msgs = delivered(&f.app, QueueKind::Usage).await;
    assert_eq!(usage_msgs.len(), 1, "{usage_msgs:?}");
    let u = &usage_msgs[0];
    assert_eq!(u["billing_outcome"], "completed");
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["terminal_state"], "completed");
    assert_eq!(u["dedupe_key"], dedupe_key(&f.turn));
    assert_eq!(u["policy_version_applied"], 1);
    assert_eq!(u["tenant_id"], f.turn.tenant_id.to_string());
    assert_eq!(u["user_id"], USER.user_id.to_string());
    assert_eq!(u["chat_id"], f.turn.chat_id.to_string());
    assert_eq!(u["turn_id"], f.turn.id.to_string());
    assert_eq!(u["request_id"], f.turn.request_id.to_string());
    assert_eq!(u["effective_model"], MODEL);
    assert_eq!(u["selected_model"], MODEL);
    assert_eq!(u["actual_credits_micro"], committed);
    assert_eq!(
        u["usage"],
        json!({"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 7,
               "cache_write_input_tokens": 0, "reasoning_tokens": 3})
    );
    assert_eq!(u["web_search_calls"], 1);
    assert_eq!(u["code_interpreter_calls"], 0);
    assert_eq!(u["file_search_calls"], 3);
    assert_eq!(u["requester_type"], "user");
    assert!(u.get("system_task_type").is_none());
    let ts = u["timestamp"].as_str().unwrap();
    assert!(ts.ends_with('Z'), "{ts}");
    chrono::DateTime::parse_from_rfc3339(ts).unwrap();

    let audit = delivered(&f.app, QueueKind::Audit).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    let a = &audit[0];
    assert_eq!(a["event_type"], "turn_completed");
    assert_eq!(a["tenant_id"], f.turn.tenant_id.to_string());
    assert_eq!(a["user_id"], USER.user_id.to_string());
    assert_eq!(a["chat_id"], f.turn.chat_id.to_string());
    assert_eq!(a["turn_id"], f.turn.id.to_string());
    assert_eq!(a["request_id"], f.turn.request_id.to_string());
    assert_eq!(a["selected_model"], MODEL);
    assert_eq!(a["effective_model"], MODEL);
    assert_eq!(a["usage"]["input_tokens"], 100);
    assert_eq!(a["usage"]["output_tokens"], 20);
    assert_eq!(a["latency_ms"], 1234);
    assert_eq!(
        a["tool_calls"],
        json!({"web_search_calls": 1, "file_search_calls": 3})
    );
    assert_eq!(
        a["policy_decisions"]["quota"],
        json!({"decision": "downgrade", "downgrade_from": MODEL,
               "downgrade_reason": "premium_quota_exhausted"})
    );
    assert_eq!(a["prompt"], "");
    assert_eq!(a["response"], "");
    assert_eq!(a["attachments"], json!([]));
    assert_eq!(a["license"], Value::Null);
    assert_eq!(a["quota_scope"], Value::Null);
    assert!(a.get("error_code").is_none());

    // No thread summary requested.
    f.app
        .assert_no_outbox(QueueKind::ThreadSummary, Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn second_finalize_is_lost_without_side_effects() {
    let f = fixture().await;
    let first = input(&f, completed(), "first", Some(usage(100, 20)));
    let first_msg = first.assistant_message_id;
    assert!(matches!(
        f.app.services.finalization.finalize(first).await,
        FinalizeResult::Won { .. }
    ));
    assert_eq!(delivered(&f.app, QueueKind::Usage).await.len(), 1);
    let quota_before = quota_rows(&f.app.db).await;

    // A competing cancel and a second completion both lose.
    let cancel = input(&f, TerminalKind::Cancelled, "partial", None);
    let cancel_msg = cancel.assistant_message_id;
    assert_eq!(
        f.app.services.finalization.finalize(cancel).await,
        FinalizeResult::Lost
    );
    let again = input(&f, completed(), "again", Some(usage(1, 1)));
    let again_msg = again.assistant_message_id;
    assert_eq!(
        f.app.services.finalization.finalize(again).await,
        FinalizeResult::Lost
    );

    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(first_msg));
    assert!(message_row(&f.app.db, cancel_msg).await.is_none());
    assert!(message_row(&f.app.db, again_msg).await.is_none());
    assert_eq!(quota_rows(&f.app.db).await, quota_before);
    assert_eq!(delivered(&f.app, QueueKind::Usage).await.len(), 1);
    assert_eq!(delivered(&f.app, QueueKind::Audit).await.len(), 1);
}

#[tokio::test]
async fn failed_without_usage_settles_estimated() {
    let f = fixture().await;
    let inp = input(
        &f,
        failed(error_codes::PROVIDER_ERROR),
        "partial text",
        None,
    );
    let msg_id = inp.assistant_message_id;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(
        matches!(
            res,
            FinalizeResult::Won {
                state: TurnState::Failed,
                ..
            }
        ),
        "{res:?}"
    );

    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(turn.error_detail.as_deref(), Some("upstream said no"));
    assert!(turn.assistant_message_id.is_none());
    assert!(message_row(&f.app.db, msg_id).await.is_none());

    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, ESTIMATED);
        assert_eq!(r.calls, 1);
        assert_eq!((r.input_tokens, r.output_tokens), (0, 0));
        assert_eq!(r.web_search_calls, 1);
    }
    for r in bucket_rows(&f.app.db, Bucket::TierPremium).await {
        assert_eq!(r.spent_credits_micro, ESTIMATED);
        assert_eq!(r.reserved_credits_micro, 0);
    }

    let u = &delivered(&f.app, QueueKind::Usage).await[0];
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["billing_outcome"], "failed");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["actual_credits_micro"], ESTIMATED);
    assert!(u.get("error_code").is_none());

    let a = &delivered(&f.app, QueueKind::Audit).await[0];
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "provider_error");
    assert_eq!(a["usage"]["input_tokens"], 0);
    assert_eq!(a["policy_decisions"]["quota"], json!({"decision": "allow"}));
}

#[tokio::test]
async fn failed_with_usage_settles_actual() {
    let f = fixture().await;
    let inp = input(
        &f,
        failed(error_codes::PROVIDER_ERROR),
        "",
        Some(usage(200, 10)),
    );
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(matches!(
        res,
        FinalizeResult::Won {
            state: TurnState::Failed,
            ..
        }
    ));

    let committed = 200 + 3 * 10;
    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, committed);
        assert_eq!((r.input_tokens, r.output_tokens), (200, 10));
    }
    let u = &delivered(&f.app, QueueKind::Usage).await[0];
    assert_eq!(u["billing_outcome"], "failed");
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["actual_credits_micro"], committed);
    assert_eq!(u["usage"]["input_tokens"], 200);
    assert_eq!(u["usage"]["output_tokens"], 10);
}

#[tokio::test]
async fn cancelled_settles_estimated_aborted_and_persists_partial_text() {
    let f = fixture().await;
    let mut inp = input(&f, TerminalKind::Cancelled, "partial ans", None);
    inp.provider_response_id = None;
    let msg_id = inp.assistant_message_id;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(
        matches!(
            res,
            FinalizeResult::Won {
                state: TurnState::Cancelled,
                ..
            }
        ),
        "{res:?}"
    );

    let msg = message_row(&f.app.db, msg_id)
        .await
        .expect("partial message");
    assert_eq!(msg.content, "partial ans");
    assert_eq!(msg.role, "assistant");
    assert_eq!(msg.model.as_deref(), Some(MODEL));
    assert_eq!((msg.input_tokens, msg.output_tokens), (0, 0));

    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, Some(msg_id));
    assert!(turn.error_code.is_none());

    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, ESTIMATED);
    }
    let u = &delivered(&f.app, QueueKind::Usage).await[0];
    assert_eq!(u["terminal_state"], "cancelled");
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["actual_credits_micro"], ESTIMATED);
    let a = &delivered(&f.app, QueueKind::Audit).await[0];
    assert_eq!(a["event_type"], "turn_failed");
    assert!(a.get("error_code").is_none());
}

#[tokio::test]
async fn cancelled_with_empty_text_has_no_message() {
    let f = fixture().await;
    let inp = input(&f, TerminalKind::Cancelled, "", None);
    let msg_id = inp.assistant_message_id;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(matches!(
        res,
        FinalizeResult::Won {
            state: TurnState::Cancelled,
            ..
        }
    ));
    assert!(message_row(&f.app.db, msg_id).await.is_none());
    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "cancelled");
    assert!(turn.assistant_message_id.is_none());
    assert_eq!(delivered(&f.app, QueueKind::Usage).await.len(), 1);
}

#[tokio::test]
async fn cancelled_partial_message_failure_still_cancels_with_null_message() {
    let f = fixture().await;
    // Occupy the pre-allocated id so the partial insert cannot succeed.
    let taken = insert_message_with(
        &f.app.db,
        NewMessage::new(f.turn.chat_id, "user", "occupied", None, now_utc()),
    )
    .await;
    let mut inp = input(&f, TerminalKind::Cancelled, "partial", None);
    inp.assistant_message_id = taken;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(
        matches!(
            res,
            FinalizeResult::Won {
                state: TurnState::Cancelled,
                ..
            }
        ),
        "{res:?}"
    );
    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "cancelled");
    assert!(turn.assistant_message_id.is_none());
    assert_eq!(
        message_row(&f.app.db, taken).await.unwrap().content,
        "occupied"
    );
    assert_eq!(delivered(&f.app, QueueKind::Usage).await.len(), 1);
}

#[tokio::test]
async fn empty_completed_text_still_persists_message() {
    let f = fixture().await;
    let inp = input(
        &f,
        TerminalKind::Completed {
            incomplete_reason: Some("max_tokens".to_owned()),
        },
        "",
        None,
    );
    let msg_id = inp.assistant_message_id;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(matches!(
        res,
        FinalizeResult::Won {
            state: TurnState::Completed,
            ..
        }
    ));
    let msg = message_row(&f.app.db, msg_id).await.expect("empty message");
    assert_eq!(msg.content, "");
    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(msg_id));
    // The incomplete reason never reaches error_code.
    assert!(turn.error_code.is_none());
    // Completed without usage: actual with zero credits, usage null.
    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, 0);
    }
    let u = &delivered(&f.app, QueueKind::Usage).await[0];
    assert_eq!(u["settlement_method"], "actual");
    assert_eq!(u["usage"], Value::Null);
    assert_eq!(u["actual_credits_micro"], 0);
}

#[tokio::test]
async fn completed_message_insert_failure_fails_turn_with_persistence_code() {
    let f = fixture().await;
    let taken = insert_message_with(
        &f.app.db,
        NewMessage::new(f.turn.chat_id, "user", "occupied", None, now_utc()),
    )
    .await;
    let mut inp = input(&f, completed(), "full answer", Some(usage(100, 20)));
    inp.assistant_message_id = taken;
    inp.thread_summary = Some(summary_payload(&f));
    assert_eq!(
        f.app.services.finalization.finalize(inp).await,
        FinalizeResult::PersistenceFailed
    );

    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert!(turn.assistant_message_id.is_none());

    let committed = 100 + 3 * 20;
    for r in bucket_rows(&f.app.db, Bucket::Total).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.spent_credits_micro, committed);
    }
    let usage_msgs = delivered(&f.app, QueueKind::Usage).await;
    assert_eq!(usage_msgs.len(), 1);
    assert_eq!(usage_msgs[0]["terminal_state"], "failed");
    assert_eq!(usage_msgs[0]["billing_outcome"], "failed");
    assert_eq!(usage_msgs[0]["settlement_method"], "actual");
    let a = &delivered(&f.app, QueueKind::Audit).await[0];
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "message_persistence_failed");
    f.app
        .assert_no_outbox(QueueKind::ThreadSummary, Duration::from_millis(300))
        .await;
}

#[tokio::test]
async fn missing_effective_model_in_snapshot_is_tx_failed_and_changes_nothing() {
    let f = fixture_with_model("not-in-catalog").await;
    let inp = input(&f, completed(), "text", Some(usage(1, 1)));
    let msg_id = inp.assistant_message_id;
    let quota_before = quota_rows(&f.app.db).await;
    assert_eq!(
        f.app.services.finalization.finalize(inp).await,
        FinalizeResult::TxFailed
    );
    let turn = turn_row(&f.app.db, f.turn.id).await;
    assert_eq!(turn.state, "running");
    assert!(message_row(&f.app.db, msg_id).await.is_none());
    assert_eq!(quota_rows(&f.app.db).await, quota_before);
    f.app
        .assert_no_outbox(QueueKind::Usage, Duration::from_millis(300))
        .await;
}

#[tokio::test]
async fn null_reserve_fields_skip_settlement_but_emit_usage() {
    let f = fixture().await;
    let mut inp = input(&f, TerminalKind::Cancelled, "", None);
    inp.turn.reserve_tokens = None;
    inp.turn.reserved_credits_micro = None;
    inp.turn.max_output_tokens_applied = None;
    inp.turn.minimal_generation_floor_applied = None;
    inp.turn.policy_version_applied = None;
    inp.turn.effective_model = None;
    let quota_before = quota_rows(&f.app.db).await;
    let res = f.app.services.finalization.finalize(inp).await;
    assert!(
        matches!(
            res,
            FinalizeResult::Won {
                state: TurnState::Cancelled,
                ..
            }
        ),
        "{res:?}"
    );
    assert_eq!(quota_rows(&f.app.db).await, quota_before);
    let u = &delivered(&f.app, QueueKind::Usage).await[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["policy_version_applied"], 0);
    assert_eq!(u["effective_model"], "");
}

fn summary_payload(f: &Fixture) -> ThreadSummaryPayload {
    ThreadSummaryPayload {
        tenant_id: f.turn.tenant_id,
        chat_id: f.turn.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: now_utc(),
        frozen_target_message_id: Uuid::new_v4(),
        system_task_type: THREAD_SUMMARY_TASK_TYPE.to_owned(),
    }
}

#[tokio::test]
async fn thread_summary_enqueued_with_completed_turn() {
    let f = fixture().await;
    let payload = summary_payload(&f);
    let mut inp = input(&f, completed(), "answer", Some(usage(10, 5)));
    inp.thread_summary = Some(payload.clone());
    assert!(matches!(
        f.app.services.finalization.finalize(inp).await,
        FinalizeResult::Won { .. }
    ));
    let msgs = delivered(&f.app, QueueKind::ThreadSummary).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0], serde_json::to_value(&payload).unwrap());
}

#[tokio::test]
async fn quota_status_returned_on_win() {
    let f = fixture().await;
    let inp = input(&f, completed(), "answer", Some(usage(100, 20)));
    let FinalizeResult::Won { quota_status, .. } = f.app.services.finalization.finalize(inp).await
    else {
        panic!("expected Won");
    };
    let tiers: Vec<_> = quota_status.iter().map(|t| t.tier).collect();
    assert_eq!(tiers, vec![QuotaTier::Premium, QuotaTier::Total]);
    let committed = 100 + 3 * 20;
    for tier in &quota_status {
        assert_eq!(tier.periods.len(), 2, "{tier:?}");
        for p in &tier.periods {
            assert_eq!(p.used, committed, "{tier:?}");
        }
    }
    let premium_daily = &quota_status[0].periods[0];
    assert_eq!(premium_daily.period, PeriodType::Daily);
    assert_eq!(premium_daily.limit, 50_000_000);
}
