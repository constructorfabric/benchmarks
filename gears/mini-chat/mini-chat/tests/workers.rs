//! Orphan turn watchdog, upload reaper and leader election (DESIGN §4 "Orphan
//! Turn Watchdog", B.9.1, B.9.5, §5.7 "`FinalizeTurn` Invariant", spec §13.2).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use mini_chat::domain::clock::now_utc;
use mini_chat::domain::estimation::period_starts;
use mini_chat::domain::services::quota::estimated_credits;
use mini_chat::infra::db::entities::{attachment, chat_turn, quota_usage};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::infra::workers::leader::{
    LeaderElector, NoopElector, ORPHAN_WATCHDOG_ROLE, UPLOAD_REAPER_ROLE,
};
use mini_chat::testing::catalog::default_catalog;
use mini_chat::testing::seed::{self, NewAttachment};
use mini_chat::testing::{DropHandle, ScriptedStream, TestApp, TestUser};
use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

fn uuid_of(v: &Value) -> Uuid {
    Uuid::parse_str(v.as_str().expect("uuid string")).expect("uuid")
}

async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A turn whose provider stream is held after `partial ` was delivered: the turn
/// is `running` and its client stream is still open.
struct HeldTurn {
    turn_id: Uuid,
    request_id: Uuid,
    conn: DropHandle,
}

async fn held_turn(app: &TestApp) -> HeldTurn {
    let chat = create_chat(app).await;
    // created(0), delta "partial "(1), delta "rest"(2): hold before 2.
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(2),
        ..ScriptedStream::text(&["partial ", "rest"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(
            U,
            &format!("{CHATS}/{chat}/messages:stream"),
            json!({"content": "hello"}),
            2,
        )
        .await;
    let request_id = uuid_of(&events[0].1["request_id"]);
    let turn = turn_by_request(app, request_id).await;
    assert_eq!(turn.state, "running");
    HeldTurn {
        turn_id: turn.id,
        request_id,
        conn,
    }
}

async fn turn_by_request(app: &TestApp, request_id: Uuid) -> chat_turn::Model {
    let conn = app.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::RequestId.eq(request_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("turn of request")
}

async fn turn(app: &TestApp, id: Uuid) -> chat_turn::Model {
    let conn = app.db.conn().unwrap();
    chat_turn::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("turn")
}

async fn set_turn(app: &TestApp, id: Uuid, cols: Vec<(chat_turn::Column, SimpleExpr)>) {
    let mut update = chat_turn::Entity::update_many();
    for (col, value) in cols {
        update = update.col_expr(col, value);
    }
    let conn = app.db.conn().unwrap();
    update
        .filter(chat_turn::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

fn secs_ago(secs: i64) -> DateTime<Utc> {
    now_utc() - ChronoDuration::seconds(secs)
}

/// Make the turn look abandoned: no progress for 400 s (timeout is 300 s).
async fn backdate_progress(app: &TestApp, id: Uuid) {
    set_turn(
        app,
        id,
        vec![(
            chat_turn::Column::LastProgressAt,
            Expr::value(secs_ago(400)),
        )],
    )
    .await;
}

async fn quota_rows(app: &TestApp) -> Vec<quota_usage::Model> {
    let conn = app.db.conn().unwrap();
    let mut rows = quota_usage::Entity::find()
        .filter(quota_usage::Column::UserId.eq(U.user_id))
        .order_by_asc(quota_usage::Column::PeriodStart)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by(|a, b| {
        (&a.bucket, &a.period_type, a.period_start).cmp(&(
            &b.bucket,
            &b.period_type,
            b.period_start,
        ))
    });
    rows
}

/// Expected estimated credits of the turn (premium catalog entry).
fn expected_estimated(t: &chat_turn::Model) -> i64 {
    let entry = &default_catalog()[0];
    let credits = estimated_credits(
        t.reserve_tokens.unwrap(),
        i64::from(t.max_output_tokens_applied.unwrap()),
        i64::from(t.minimal_generation_floor_applied.unwrap()),
        entry,
    )
    .unwrap();
    assert!(credits > 0, "the estimate must be observable");
    credits
}

async fn usage_events(app: &TestApp, n: usize) -> Vec<Value> {
    app.outbox_messages_n(QueueKind::Usage, n).await
}

async fn audit_events(app: &TestApp, n: usize) -> Vec<Value> {
    app.outbox_messages_n(QueueKind::Audit, n).await
}

// ---------------------------------------------------------------------------------------------
// leader election
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn noop_elector_is_always_leader() {
    let elector = NoopElector;
    assert!(elector.is_leader(ORPHAN_WATCHDOG_ROLE).await);
    assert!(elector.is_leader(UPLOAD_REAPER_ROLE).await);
    assert!(elector.is_leader("anything").await);
    assert_eq!(ORPHAN_WATCHDOG_ROLE, "orphan-watchdog");
    assert_eq!(UPLOAD_REAPER_ROLE, "upload-reaper");
}

/// Elector with a fixed answer that records the roles it was asked about.
struct FixedElector {
    leader: bool,
    roles: Mutex<Vec<String>>,
}

impl FixedElector {
    fn new(leader: bool) -> Arc<Self> {
        Arc::new(Self {
            leader,
            roles: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<String> {
        self.roles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LeaderElector for FixedElector {
    async fn is_leader(&self, role: &str) -> bool {
        self.roles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(role.to_owned());
        self.leader
    }
}

async fn fast_scan_app(elector: Arc<FixedElector>) -> TestApp {
    TestApp::builder()
        .config(|c| {
            c.orphan_watchdog.scan_interval_secs = 1;
            c.upload_reaper.scan_interval_secs = 1;
        })
        .elector(elector)
        .build()
        .await
}

#[tokio::test]
async fn spawned_watchdog_scans_as_leader_and_stops_on_cancel() {
    let elector = FixedElector::new(true);
    let app = fast_scan_app(Arc::clone(&elector)).await;
    let held = held_turn(&app).await;
    backdate_progress(&app, held.turn_id).await;

    let cancel = CancellationToken::new();
    let handle = Arc::clone(&app.services.orphan_watchdog).spawn(cancel.clone());
    eventually("watchdog fails the stale turn", || async {
        turn(&app, held.turn_id).await.state == "failed"
    })
    .await;
    assert_eq!(
        turn(&app, held.turn_id).await.error_code.as_deref(),
        Some("orphan_timeout")
    );
    assert!(elector.asked().iter().all(|r| r == ORPHAN_WATCHDOG_ROLE));

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("watchdog stops on cancel")
        .unwrap();
}

#[tokio::test]
async fn spawned_watchdog_and_reaper_do_nothing_when_not_leader() {
    let elector = FixedElector::new(false);
    let app = fast_scan_app(Arc::clone(&elector)).await;
    let held = held_turn(&app).await;
    backdate_progress(&app, held.turn_id).await;
    let chat = create_chat(&app).await;
    let att = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "a.pdf").status("pending"),
    )
    .await;
    set_attachment(
        &app,
        att,
        vec![(attachment::Column::UpdatedAt, Expr::value(secs_ago(900)))],
    )
    .await;

    let cancel = CancellationToken::new();
    let watchdog = Arc::clone(&app.services.orphan_watchdog).spawn(cancel.clone());
    let reaper = Arc::clone(&app.services.upload_reaper).spawn(cancel.clone());
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(turn(&app, held.turn_id).await.state, "running");
    assert_eq!(attachment_row(&app, att).await.status, "pending");
    let asked = elector.asked();
    assert!(asked.iter().any(|r| r == ORPHAN_WATCHDOG_ROLE), "{asked:?}");
    assert!(asked.iter().any(|r| r == UPLOAD_REAPER_ROLE), "{asked:?}");

    cancel.cancel();
    for handle in [watchdog, reaper] {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker stops on cancel")
            .unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// orphan watchdog
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn watchdog_fails_stale_running_turn_with_estimated_settlement() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    let before = turn(&app, held.turn_id).await;
    let expected = expected_estimated(&before);
    let reserved = before.reserved_credits_micro.unwrap();
    let rows = quota_rows(&app).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows.iter().all(|r| r.reserved_credits_micro == reserved));
    backdate_progress(&app, held.turn_id).await;

    let now = now_utc();
    let n = app.services.orphan_watchdog.scan_once(now).await.unwrap();
    assert_eq!(n, 1);

    let t = turn(&app, held.turn_id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(t.completed_at, Some(now));
    assert_eq!(t.updated_at, Some(now));

    // Reserve released, spent = estimated credits, one settlement per bucket row.
    let rows = quota_rows(&app).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for row in &rows {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.spent_credits_micro, expected, "{row:?}");
        assert_eq!(row.calls, 1, "{row:?}");
    }

    let usage = usage_events(&app, 1).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = &usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["selected_model"], "gpt-premium");
    assert_eq!(u["effective_model"], u["selected_model"]);
    assert!(u["usage"].is_null(), "{u}");
    assert_eq!(u["actual_credits_micro"], expected);
    assert_eq!(
        u["policy_version_applied"],
        before.policy_version_applied.unwrap()
    );
    assert_eq!(u["user_id"], json!(U.user_id));
    assert_eq!(uuid_of(&u["turn_id"]), held.turn_id);
    assert_eq!(uuid_of(&u["request_id"]), held.request_id);
    assert_eq!(
        u["dedupe_key"],
        format!(
            "{}/{}/{}",
            U.tenant_id.simple(),
            held.turn_id.simple(),
            held.request_id.simple()
        )
    );

    let audit = audit_events(&app, 1).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    let a = &audit[0];
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "orphan_timeout");
    assert_eq!(a["policy_decisions"]["quota"]["decision"], "unknown");
    assert_eq!(a["selected_model"], "gpt-premium");
    assert_eq!(a["effective_model"], "gpt-premium");
    assert_eq!(a["user_id"], json!(U.user_id));

    // The still-open client stream ends with `stream_interrupted` once the
    // provider task resumes and loses the finalization CAS.
    app.provider.release();
    let rest = held.conn.rest().await;
    let (name, data) = rest.last().expect("events after the release");
    assert_eq!(name, "error", "{rest:?}");
    assert_eq!(data["code"], "stream_interrupted", "{rest:?}");

    // Nothing else was committed by the losing finalizer.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(usage_events(&app, 1).await.len(), 1);
    assert_eq!(audit_events(&app, 1).await.len(), 1);
    let t = turn(&app, held.turn_id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(quota_rows(&app).await[0].calls, 1);
}

/// A scan window's worth (100) of orphans whose finalization fails permanently
/// (their effective model is missing from the policy snapshot), all older than
/// a finalizable orphan: the failing ones are held back after failing, so the
/// next scan reaches the finalizable one.
#[tokio::test]
async fn watchdog_permanently_failing_orphans_do_not_starve_other_stale_turns() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    backdate_progress(&app, held.turn_id).await;
    let template = turn(&app, held.turn_id).await;
    let conn = app.db.conn().unwrap();
    let mut failing = Vec::new();
    for _ in 0..100 {
        let chat = create_chat(&app).await;
        let row = chat_turn::Model {
            id: Uuid::new_v4(),
            chat_id: chat,
            request_id: Uuid::new_v4(),
            effective_model: Some("model-missing-from-snapshot".to_owned()),
            started_at: secs_ago(2000),
            last_progress_at: Some(secs_ago(2000)),
            ..template.clone()
        };
        failing.push(row.id);
        secure_insert::<chat_turn::Entity>(
            row.into_active_model(),
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    let watchdog = &app.services.orphan_watchdog;
    let first = watchdog.scan_once(now_utc()).await.unwrap();
    let second = watchdog.scan_once(now_utc()).await.unwrap();
    assert_eq!((first, second), (0, 1));
    let t = turn(&app, held.turn_id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
    for id in failing {
        assert_eq!(turn(&app, id).await.state, "running");
    }
}

#[tokio::test]
async fn watchdog_skips_fresh_and_terminal_turns() {
    let app = TestApp::builder().build().await;
    // A running turn that made progress 100 s ago (timeout 300 s).
    let fresh = held_turn(&app).await;
    set_turn(
        &app,
        fresh.turn_id,
        vec![(
            chat_turn::Column::LastProgressAt,
            Expr::value(secs_ago(100)),
        )],
    )
    .await;

    // A completed turn with a stale progress timestamp.
    let chat = create_chat(&app).await;
    app.provider
        .push_stream(ScriptedStream::text(&["hi"], 10, 5));
    let c = app
        .stream(
            U,
            &format!("{CHATS}/{chat}/messages:stream"),
            json!({"content": "done"}),
        )
        .await;
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    let done = turn_by_request(&app, uuid_of(&c.events[0].1["request_id"])).await;
    assert_eq!(done.state, "completed");
    set_turn(
        &app,
        done.id,
        vec![(
            chat_turn::Column::LastProgressAt,
            Expr::value(secs_ago(1000)),
        )],
    )
    .await;
    let usage_before = usage_events(&app, 1).await.len();
    let quota_before = quota_rows(&app).await;

    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        0
    );
    assert_eq!(turn(&app, fresh.turn_id).await.state, "running");
    let t = turn(&app, done.id).await;
    assert_eq!(t.state, "completed");
    assert_eq!(t.error_code, None);
    assert_eq!(usage_events(&app, 1).await.len(), usage_before);
    assert_eq!(quota_rows(&app).await, quota_before);

    // A stale but soft-deleted running turn is left alone as well.
    backdate_progress(&app, fresh.turn_id).await;
    set_turn(
        &app,
        fresh.turn_id,
        vec![(chat_turn::Column::DeletedAt, Expr::value(now_utc()))],
    )
    .await;
    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        0
    );
    assert_eq!(turn(&app, fresh.turn_id).await.state, "running");
}

#[tokio::test]
async fn watchdog_null_progress_uses_started_at() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    // NULL progress with a recent start: not an orphan.
    set_turn(
        &app,
        held.turn_id,
        vec![(
            chat_turn::Column::LastProgressAt,
            Expr::value(Option::<DateTime<Utc>>::None),
        )],
    )
    .await;
    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        0
    );
    assert_eq!(turn(&app, held.turn_id).await.state, "running");

    // NULL progress with a start older than the timeout: an orphan.
    set_turn(
        &app,
        held.turn_id,
        vec![(chat_turn::Column::StartedAt, Expr::value(secs_ago(400)))],
    )
    .await;
    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        1
    );
    let t = turn(&app, held.turn_id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
}

#[tokio::test]
async fn watchdog_null_reserve_fields_skips_settlement_but_emits_usage() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    let quota_before = quota_rows(&app).await;
    set_turn(
        &app,
        held.turn_id,
        vec![
            (
                chat_turn::Column::ReserveTokens,
                Expr::value(Option::<i64>::None),
            ),
            (
                chat_turn::Column::MaxOutputTokensApplied,
                Expr::value(Option::<i32>::None),
            ),
            (
                chat_turn::Column::ReservedCreditsMicro,
                Expr::value(Option::<i64>::None),
            ),
            (
                chat_turn::Column::PolicyVersionApplied,
                Expr::value(Option::<i64>::None),
            ),
            (
                chat_turn::Column::EffectiveModel,
                Expr::value(Option::<String>::None),
            ),
            (
                chat_turn::Column::MinimalGenerationFloorApplied,
                Expr::value(Option::<i32>::None),
            ),
            (
                chat_turn::Column::LastProgressAt,
                Expr::value(secs_ago(400)),
            ),
        ],
    )
    .await;

    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        1
    );
    let t = turn(&app, held.turn_id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
    // No quota_usage row changed.
    assert_eq!(quota_rows(&app).await, quota_before);

    let usage = usage_events(&app, 1).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = &usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["actual_credits_micro"], 0);
    assert_eq!(u["policy_version_applied"], 0);
    assert!(u["usage"].is_null(), "{u}");
    assert_eq!(u["effective_model"], "");
    assert_eq!(u["selected_model"], "");

    let audit = audit_events(&app, 1).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["event_type"], "turn_failed");
    assert_eq!(audit[0]["policy_decisions"]["quota"]["decision"], "unknown");
}

#[tokio::test]
async fn watchdog_null_requester_skips_settlement_and_reports_no_user() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    let quota_before = quota_rows(&app).await;
    set_turn(
        &app,
        held.turn_id,
        vec![
            (
                chat_turn::Column::RequesterUserId,
                Expr::value(Option::<Uuid>::None),
            ),
            (
                chat_turn::Column::LastProgressAt,
                Expr::value(secs_ago(400)),
            ),
        ],
    )
    .await;

    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        1
    );
    assert_eq!(turn(&app, held.turn_id).await.state, "failed");
    assert_eq!(quota_rows(&app).await, quota_before);

    let usage = usage_events(&app, 1).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = &usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert!(u.get("user_id").is_none(), "{u}");
    // The reserve fields are intact, so the models are reported.
    assert_eq!(u["effective_model"], "gpt-premium");
    assert_eq!(u["selected_model"], "gpt-premium");

    let audit = audit_events(&app, 1).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["event_type"], "turn_failed");
    assert_eq!(audit[0]["user_id"], json!(Uuid::nil()));
}

#[tokio::test]
async fn watchdog_settles_against_the_periods_of_started_at() {
    let app = TestApp::builder().build().await;
    let held = held_turn(&app).await;
    let before = turn(&app, held.turn_id).await;
    let expected = expected_estimated(&before);

    // The turn started 40 days ago: its reserve lives in that day's and month's rows.
    let started = secs_ago(40 * 24 * 3600);
    let (daily, monthly) = period_starts(started);
    let (today_daily, today_monthly) = period_starts(now_utc());
    assert_ne!(daily, today_daily);
    assert_ne!(monthly, today_monthly);
    let conn = app.db.conn().unwrap();
    for (period_type, start) in [("daily", daily), ("monthly", monthly)] {
        quota_usage::Entity::update_many()
            .col_expr(quota_usage::Column::PeriodStart, Expr::value(start))
            .filter(quota_usage::Column::PeriodType.eq(period_type))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }
    set_turn(
        &app,
        held.turn_id,
        vec![
            (chat_turn::Column::StartedAt, Expr::value(started)),
            (
                chat_turn::Column::LastProgressAt,
                Expr::value(secs_ago(400)),
            ),
        ],
    )
    .await;

    assert_eq!(
        app.services
            .orphan_watchdog
            .scan_once(now_utc())
            .await
            .unwrap(),
        1
    );
    let rows = quota_rows(&app).await;
    assert_eq!(rows.len(), 4, "no row of the current periods: {rows:?}");
    for row in &rows {
        let start = if row.period_type == "daily" {
            daily
        } else {
            monthly
        };
        assert_eq!(row.period_start, start, "{row:?}");
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.spent_credits_micro, expected, "{row:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// upload reaper
// ---------------------------------------------------------------------------------------------

async fn attachment_row(app: &TestApp, id: Uuid) -> attachment::Model {
    let conn = app.db.conn().unwrap();
    attachment::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment")
}

async fn set_attachment(app: &TestApp, id: Uuid, cols: Vec<(attachment::Column, SimpleExpr)>) {
    let mut update = attachment::Entity::update_many();
    for (col, value) in cols {
        update = update.col_expr(col, value);
    }
    let conn = app.db.conn().unwrap();
    update
        .filter(attachment::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Seed an attachment of `status` last updated `age_secs` ago; `file` is its
/// provider file id (`None` = the upload never reached the provider).
async fn seed_attachment(
    app: &TestApp,
    chat: Uuid,
    status: &str,
    age_secs: i64,
    file: Option<&str>,
) -> Uuid {
    let id = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "doc.pdf").status(status),
    )
    .await;
    set_attachment(
        app,
        id,
        vec![
            (
                attachment::Column::UpdatedAt,
                Expr::value(secs_ago(age_secs)),
            ),
            (
                attachment::Column::ProviderFileId,
                Expr::value(file.map(str::to_owned)),
            ),
        ],
    )
    .await;
    id
}

#[tokio::test]
async fn reaper_fails_stale_pending_and_uploaded_rows() {
    let app = TestApp::builder().build().await;
    let chat = create_chat(&app).await;
    // Stale: pending without provider file, uploaded with one.
    let pending = seed_attachment(&app, chat, "pending", 900, None).await;
    let uploaded = seed_attachment(&app, chat, "uploaded", 900, Some("file-abandoned")).await;
    // Skipped: fresh, ready, soft-deleted, owned by cleanup.
    let fresh = seed_attachment(&app, chat, "uploaded", 10, Some("file-fresh")).await;
    let ready = seed_attachment(&app, chat, "ready", 900, Some("file-ready")).await;
    let deleted = seed_attachment(&app, chat, "uploaded", 900, Some("file-deleted")).await;
    set_attachment(
        &app,
        deleted,
        vec![(attachment::Column::DeletedAt, Expr::value(now_utc()))],
    )
    .await;
    let owned = seed_attachment(&app, chat, "uploaded", 900, Some("file-owned")).await;
    set_attachment(
        &app,
        owned,
        vec![(attachment::Column::CleanupStatus, Expr::value("pending"))],
    )
    .await;

    let now = now_utc();
    assert_eq!(app.services.upload_reaper.scan_once(now).await.unwrap(), 2);

    let p = attachment_row(&app, pending).await;
    assert_eq!(p.status, "failed");
    assert_eq!(p.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(p.cleanup_status, None);
    assert_eq!(p.deleted_at, None);
    assert_eq!(p.updated_at, now);

    let u = attachment_row(&app, uploaded).await;
    assert_eq!(u.status, "failed");
    assert_eq!(u.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(u.deleted_at, None);
    assert_eq!(u.updated_at, now);
    assert_eq!(u.cleanup_updated_at, Some(now));
    // The provider file is handed to cleanup: the cleanup handler may already
    // have finished it by now.
    assert!(
        matches!(u.cleanup_status.as_deref(), Some("pending" | "done")),
        "{u:?}"
    );

    for (id, status) in [
        (fresh, "uploaded"),
        (ready, "ready"),
        (deleted, "uploaded"),
        (owned, "uploaded"),
    ] {
        let row = attachment_row(&app, id).await;
        assert_eq!(row.status, status, "{row:?}");
        assert_eq!(row.error_code, None, "{row:?}");
    }
    assert_eq!(
        attachment_row(&app, owned).await.cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(attachment_row(&app, fresh).await.cleanup_status, None);

    // Exactly one cleanup event: the uploaded row that has a provider file.
    let cleanup = app.outbox_messages_n(QueueKind::AttachmentCleanup, 1).await;
    assert_eq!(cleanup.len(), 1, "{cleanup:?}");
    let c = &cleanup[0];
    assert_eq!(c["event_type"], "attachment_upload_abandoned");
    assert_eq!(uuid_of(&c["attachment_id"]), uploaded);
    assert_eq!(uuid_of(&c["chat_id"]), chat);
    assert_eq!(c["tenant_id"], json!(U.tenant_id));
    assert_eq!(c["provider_file_id"], "file-abandoned");
    assert_eq!(c["storage_backend"], "openai");
    assert_eq!(c["attachment_kind"], "document");
    assert!(c["secondary_ref"].is_null(), "{c}");

    // The cleanup handler finishes the provider cleanup.
    eventually("cleanup done", || async {
        attachment_row(&app, uploaded)
            .await
            .cleanup_status
            .as_deref()
            == Some("done")
    })
    .await;

    // A second scan finds nothing.
    assert_eq!(
        app.services
            .upload_reaper
            .scan_once(now_utc())
            .await
            .unwrap(),
        0
    );
}
