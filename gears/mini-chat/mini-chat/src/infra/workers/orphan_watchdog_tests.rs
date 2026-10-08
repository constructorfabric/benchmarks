//! Orphan watchdog tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::MiniChatAuditEvent;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use time::{Duration, OffsetDateTime};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::scan_once;
use crate::domain::quota::{self, BUCKET_TOTAL, PERIOD_DAILY, PreflightDecision, PreflightRequest, ToolInputs};
use crate::infra::db::entities::{chat, chat_turn, quota_usage};
use crate::testing::{STANDARD, TENANT_A, TestApp, USER_A1};

fn ago(now: OffsetDateTime, secs: i64) -> OffsetDateTime {
    crate::clock::normalize(now - Duration::seconds(secs))
}

/// Preflight decision of a standard turn started at `started_at`, with the reserve booked.
async fn reserve_at(t: &TestApp, started_at: OffsetDateTime) -> PreflightDecision {
    let req = PreflightRequest {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        selected_model: STANDARD.to_owned(),
        message_bytes: 400,
        image_count: 0,
        prior_context_tokens: 0,
        tools: ToolInputs::default(),
        now: started_at,
    };
    let d = quota::preflight(&t.app, &req).await.unwrap();
    let d2 = d.clone();
    t.app
        .db
        .transaction(move |tx| Box::pin(async move { quota::reserve(tx, TENANT_A, USER_A1, &d2).await }))
        .await
        .unwrap();
    d
}

/// Inserts a chat and a running turn.
async fn seed_turn(
    t: &TestApp,
    decision: Option<&PreflightDecision>,
    started_at: OffsetDateTime,
    last_progress_at: Option<OffsetDateTime>,
    f: impl FnOnce(&mut chat_turn::Model),
) -> chat_turn::Model {
    let conn = t.app.db.conn().unwrap();
    let scope = AccessScope::allow_all();
    let chat_id = Uuid::new_v4();
    let c = chat::ActiveModel {
        id: Set(chat_id),
        tenant_id: Set(TENANT_A),
        user_id: Set(USER_A1),
        model: Set(STANDARD.to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(started_at),
        updated_at: Set(started_at),
        deleted_at: Set(None),
    };
    chat::Entity::insert(c).secure().scope_unchecked(&scope).unwrap().exec(&conn).await.unwrap();
    let mut m = chat_turn::Model {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        chat_id,
        request_id: Uuid::new_v4(),
        requester_type: "user".to_owned(),
        requester_user_id: Some(USER_A1),
        state: "running".to_owned(),
        provider_name: None,
        provider_response_id: None,
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: decision.map(|d| d.reserve_tokens),
        max_output_tokens_applied: decision.map(|d| i32::try_from(d.max_output_tokens_applied).unwrap()),
        reserved_credits_micro: decision.map(|d| d.reserved_credits_micro),
        policy_version_applied: decision.map(|d| d.policy_version),
        effective_model: decision.map(|d| d.effective_model.id.clone()),
        minimal_generation_floor_applied: decision.map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap()),
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at,
        last_progress_at,
        web_search_enabled: false,
        web_search_completed_count: 1,
        code_interpreter_completed_count: 2,
        file_search_completed_count: 3,
        completed_at: None,
        updated_at: started_at,
    };
    f(&mut m);
    let am = chat_turn::ActiveModel::from(m.clone()).reset_all();
    chat_turn::Entity::insert(am).secure().scope_unchecked(&scope).unwrap().exec(&conn).await.unwrap();
    m
}

async fn load_turn(t: &TestApp, id: Uuid) -> chat_turn::Model {
    let conn = t.app.db.conn().unwrap();
    chat_turn::Entity::find_by_id(id).secure().scope_with(&AccessScope::allow_all()).one(&conn).await.unwrap().unwrap()
}

async fn quota_rows(t: &TestApp) -> Vec<quota_usage::Model> {
    let conn = t.app.db.conn().unwrap();
    quota_usage::Entity::find().secure().scope_with(&AccessScope::allow_all()).all(&conn).await.unwrap()
}

async fn wait_published(t: &TestApp, n: usize) {
    let policy = Arc::clone(&t.policy);
    t.eventually("usage events published", || {
        let p = Arc::clone(&policy);
        async move { p.published.lock().unwrap().len() >= n }
    })
    .await;
}

async fn wait_audited(t: &TestApp, n: usize) {
    let audit = Arc::clone(&t.audit);
    t.eventually("audit events delivered", || {
        let a = Arc::clone(&audit);
        async move { a.events.lock().unwrap().len() >= n }
    })
    .await;
}

#[tokio::test]
async fn stale_turn_is_finalized_settled_and_reported() {
    let t = TestApp::new().await;
    let now = crate::clock::now();
    let started = ago(now, 1000);
    let d = reserve_at(&t, started).await;
    let turn = seed_turn(&t, Some(&d), started, Some(ago(now, 500)), |_| {}).await;

    assert_eq!(scan_once(&t.app, now).await.unwrap(), 1);

    let row = load_turn(&t, turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert!(row.completed_at.is_some());
    assert!(row.updated_at > turn.updated_at);

    // estimated settlement on the reserve rows of the started_at periods
    let rows = quota_rows(&t).await;
    let daily = rows.iter().find(|r| r.period_type == PERIOD_DAILY && r.bucket == BUCKET_TOTAL).unwrap();
    assert_eq!(daily.period_start, quota::period_starts(started).daily);
    assert_eq!(daily.reserved_credits_micro, 0);
    assert_eq!(daily.spent_credits_micro, 370);
    assert_eq!(daily.calls, 1);
    assert_eq!((daily.web_search_calls, daily.code_interpreter_calls), (1, 2));
    assert_eq!((daily.input_tokens, daily.output_tokens), (0, 0));

    wait_published(&t, 1).await;
    let e = t.policy.published.lock().unwrap()[0].clone();
    assert_eq!(e.billing_outcome, "aborted");
    assert_eq!(e.settlement_method, "estimated");
    assert_eq!(e.terminal_state, "failed");
    assert_eq!(e.actual_credits_micro, 370);
    assert_eq!(e.effective_model, STANDARD);
    assert_eq!(e.selected_model, STANDARD);
    assert_eq!(e.policy_version_applied, 1);
    assert_eq!(e.usage, None);
    assert_eq!(e.user_id, Some(USER_A1));
    assert_eq!(e.turn_id, Some(turn.id));
    assert_eq!((e.web_search_calls, e.code_interpreter_calls, e.file_search_calls), (1, 2, 3));
    assert_eq!(e.dedupe_key, quota::dedupe_key(TENANT_A, turn.id, turn.request_id));

    wait_audited(&t, 1).await;
    let MiniChatAuditEvent::Turn(a) = t.audit.events.lock().unwrap()[0].clone() else { panic!("turn audit expected") };
    assert_eq!(a.event_type, "turn_failed");
    assert_eq!(a.terminal_state, "failed");
    assert_eq!(a.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(a.policy_decisions.quota.decision, "unknown");
    assert_eq!(a.selected_model, STANDARD);
    assert_eq!(a.effective_model, STANDARD);
    assert_eq!(a.turn_id, turn.id);
    assert_eq!((a.tool_calls.web_search_calls, a.tool_calls.file_search_calls), (1, 3));
    assert!(a.latency_ms >= 1_000_000 - 1);

    // a second scan is a no-op
    assert_eq!(scan_once(&t.app, crate::clock::now()).await.unwrap(), 0);
    let rows = quota_rows(&t).await;
    let daily = rows.iter().find(|r| r.period_type == PERIOD_DAILY && r.bucket == BUCKET_TOTAL).unwrap();
    assert_eq!(daily.calls, 1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(t.policy.published.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recent_progress_is_not_touched() {
    let t = TestApp::new().await;
    let now = crate::clock::now();
    let started = ago(now, 3600);
    let d = reserve_at(&t, started).await;
    let turn = seed_turn(&t, Some(&d), started, Some(ago(now, 10)), |_| {}).await;
    assert_eq!(scan_once(&t.app, now).await.unwrap(), 0);
    assert_eq!(load_turn(&t, turn.id).await.state, "running");
}

#[tokio::test]
async fn null_progress_falls_back_to_started_at() {
    let t = TestApp::new().await;
    let now = crate::clock::now();
    let old = seed_turn(&t, None, ago(now, 200), None, |_| {}).await;
    let fresh = seed_turn(&t, None, ago(now, 30), None, |_| {}).await;
    assert_eq!(scan_once(&t.app, now).await.unwrap(), 1);
    assert_eq!(load_turn(&t, old.id).await.state, "failed");
    assert_eq!(load_turn(&t, fresh.id).await.state, "running");
}

#[tokio::test]
async fn timeout_boundary_and_terminal_or_deleted_turns() {
    let t = TestApp::new().await;
    let now = crate::clock::now();
    // timeout is 90 s in the test config
    let inside = seed_turn(&t, None, ago(now, 89), Some(ago(now, 89)), |_| {}).await;
    let deleted = seed_turn(&t, None, ago(now, 500), Some(ago(now, 500)), |m| m.deleted_at = Some(ago(now, 1))).await;
    let done = seed_turn(&t, None, ago(now, 500), Some(ago(now, 500)), |m| m.state = "completed".to_owned()).await;
    assert_eq!(scan_once(&t.app, now).await.unwrap(), 0);
    assert_eq!(load_turn(&t, inside.id).await.state, "running");
    assert_eq!(load_turn(&t, deleted.id).await.state, "running");
    assert_eq!(load_turn(&t, done.id).await.state, "completed");
}

#[tokio::test]
async fn turn_without_reserve_is_finalized_without_settlement() {
    let t = TestApp::new().await;
    let now = crate::clock::now();
    let turn = seed_turn(&t, None, ago(now, 1000), Some(ago(now, 1000)), |_| {}).await;
    assert_eq!(scan_once(&t.app, now).await.unwrap(), 1);
    assert_eq!(load_turn(&t, turn.id).await.error_code.as_deref(), Some("orphan_timeout"));
    assert!(quota_rows(&t).await.is_empty(), "no quota row changes");
    wait_published(&t, 1).await;
    let e = t.policy.published.lock().unwrap()[0].clone();
    assert_eq!(e.billing_outcome, "aborted");
    assert_eq!(e.settlement_method, "estimated");
    assert_eq!(e.actual_credits_micro, 0);
    assert_eq!(e.usage, None);
    assert_eq!(e.effective_model, "");
    assert_eq!(e.selected_model, "");
    assert_eq!(e.policy_version_applied, 0);
    wait_audited(&t, 1).await;
}

#[tokio::test]
async fn run_respects_cancellation() {
    let t = TestApp::with_config(|c| c.orphan_watchdog.scan_interval_secs = 3600).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let handle = tokio::spawn(super::run(Arc::clone(&t.app), cancel.clone()));
    cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(2), handle).await.unwrap().unwrap();
}
