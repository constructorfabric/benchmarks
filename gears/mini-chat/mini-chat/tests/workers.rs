#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

//! Background workers (S§10.3, S§10.4): the orphan watchdog finalizes
//! stale `running` turns (orphan CAS, estimated settlement, usage + audit
//! events) and the upload reaper fails abandoned uploads (cleanup event when
//! a provider file exists).

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use mini_chat::domain::error::DomainError;
use mini_chat::infra::db::entity::{attachment, chat, chat_turn};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, TurnRepo, TurnTerminal};
use mini_chat::infra::workers::orphan_watchdog::{OrphanWatchdog, ScanReport};
use mini_chat::infra::workers::upload_reaper::{ReapReport, UploadReaper};
use mini_chat::test_support::SseStep;
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tokio::sync::Notify;
use uuid::Uuid;

use common::*;

const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
const AUDIT_QUEUE: &str = "mini-chat.audit";
const CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn now(app: &TestApp) -> OffsetDateTime {
    use mini_chat::domain::clock::Clock;
    app.clock.now()
}

/// Script a reply that sends one delta and then holds the body open.
fn push_held(app: &TestApp) -> Arc<Notify> {
    let gate = Arc::new(Notify::new());
    let (d_ev, d_data) = delta("partial");
    let (c_ev, c_data) = completed(3, 2);
    app.oagw.push_sse_script(
        PROVIDER_PATH,
        vec![
            SseStep::frame(d_ev, d_data),
            SseStep::Wait(Arc::clone(&gate)),
            SseStep::frame(c_ev, c_data),
        ],
    );
    gate
}

/// Start a send whose provider stream is held open (the turn stays
/// `running` with its reserve taken); returns the open response and the
/// turn's request id.
async fn start_held_turn(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
) -> (LiveResponse, Arc<Notify>, Uuid) {
    let gate = push_held(app);
    let rid = Uuid::new_v4();
    let mut live = client
        .open_stream(
            &stream_path(chat),
            &json!({"content": "hello", "request_id": rid}),
        )
        .await;
    assert_eq!(live.status, StatusCode::OK);
    assert_eq!(live.next_event().await.unwrap().0, "stream_started");
    assert_eq!(live.next_event().await.unwrap().0, "delta");
    (live, gate, rid)
}

async fn chat_model(app: &TestApp, tenant: Uuid, user: Uuid, id: Uuid) -> chat::Model {
    let conn = app.db.conn().unwrap();
    ChatRepo
        .find_by_id(&conn, &tenant_scope(tenant, user), id)
        .await
        .unwrap()
        .unwrap()
}

async fn insert_turn(app: &TestApp, row: chat_turn::Model) -> chat_turn::Model {
    let conn = app.db.conn().unwrap();
    TurnRepo
        .insert(&conn, &tenant_scope(row.tenant_id, Uuid::nil()), row)
        .await
        .unwrap()
}

async fn turn(app: &TestApp, id: Uuid) -> chat_turn::Model {
    chat_turn::Entity::find_by_id(id)
        .one(&app.raw)
        .await
        .unwrap()
        .unwrap()
}

/// A chat (model `s1`) and a `running` turn row of it: reserve fields of
/// `s1`, started long before the harness clock.
async fn chat_with_running_turn(app: &TestApp) -> (chat::Model, chat_turn::Model) {
    let (user, tenant) = ids();
    let chat_id = create_chat(&app.as_user(user, tenant), "s1").await;
    let c = chat_model(app, tenant, user, chat_id).await;
    let row = chat_turn::Model {
        effective_model: Some("s1".to_owned()),
        ..turn_row(&c, Uuid::new_v4(), "running")
    };
    (c, row)
}

fn watchdog(app: &TestApp) -> OrphanWatchdog {
    OrphanWatchdog::new(&app.services)
}

fn reaper(app: &TestApp) -> UploadReaper {
    UploadReaper::new(&app.services)
}

fn usage_for(payloads: &[Value], rid: Uuid) -> Vec<&Value> {
    payloads
        .iter()
        .filter(|p| p["request_id"] == rid.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Orphan watchdog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphan_finalizes_stale_running_turn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    // Selected `p1`, disabled → effective `s1` (downgrade): the orphan path
    // must report selected_model = effective_model.
    let chat = create_chat(&client, "p1").await;
    app.policy
        .set_catalog(vec![disabled(premium_model("p1")), standard_model("s1")]);
    let (_live, _gate, rid) = start_held_turn(&app, &client, chat).await;
    let running = turn_by_request(&app, rid).await;
    assert_eq!(running.state, "running");
    let reserved = running.reserved_credits_micro.unwrap();
    assert!(reserved > 0);
    assert_eq!(
        quota_row_of(&app, "daily", "total")
            .await
            .reserved_credits_micro,
        reserved
    );

    app.clock.advance(Duration::seconds(301));
    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );

    let done = turn_by_request(&app, rid).await;
    assert_eq!(done.state, "failed");
    assert_eq!(done.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(done.completed_at, Some(now(&app)));

    // Estimated settlement: est. input + min(floor, max_out), s1 multipliers
    // (1e6 / 3e6 micro per token).
    let est_input =
        done.reserve_tokens.unwrap() - i64::from(done.max_output_tokens_applied.unwrap());
    let floor = i64::from(
        done.minimal_generation_floor_applied
            .unwrap()
            .min(done.max_output_tokens_applied.unwrap()),
    );
    let expected = est_input + floor * 3;
    for bucket in ["total"] {
        for period in ["daily", "monthly"] {
            let row = quota_row_of(&app, period, bucket).await;
            assert_eq!(row.reserved_credits_micro, 0, "{period}/{bucket}");
            assert_eq!(row.spent_credits_micro, expected, "{period}/{bucket}");
        }
    }

    let usage = app.outbox_payloads(USAGE_QUEUE).await;
    let usage = usage_for(&usage, rid);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert_eq!(u["effective_model"], "s1");
    assert_eq!(u["selected_model"], u["effective_model"]);
    assert_eq!(u["actual_credits_micro"], expected);
    assert!(u["usage"].is_null(), "{u}");
    assert_eq!(
        u["dedupe_key"],
        format!("{}/{}/{}", tenant.simple(), done.id.simple(), rid.simple())
    );

    let audit = app.outbox_payloads(AUDIT_QUEUE).await;
    let audit = usage_for(&audit, rid);
    assert_eq!(audit.len(), 1, "{audit:?}");
    let a = audit[0];
    assert_eq!(a["event_type"], "turn_failed");
    assert_eq!(a["error_code"], "orphan_timeout");
    assert_eq!(a["selected_model"], "s1");
    assert_eq!(a["effective_model"], "s1");
    assert_eq!(a["policy_decisions"]["quota"]["decision"], "unknown");
}

#[tokio::test]
async fn recent_progress_not_orphaned() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    // Started long ago, but progress 10 s before now.
    let row = insert_turn(
        &app,
        chat_turn::Model {
            last_progress_at: Some(now(&app) - Duration::seconds(10)),
            ..row
        },
    )
    .await;

    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(report, ScanReport::default());
    assert_eq!(turn(&app, row.id).await.state, "running");
    assert!(app.outbox_payloads(USAGE_QUEUE).await.is_empty());
}

#[tokio::test]
async fn subsecond_progress_after_cutoff_not_orphaned() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    // Half a second newer than the cutoff (timeout 300 s): not stale.
    let row = insert_turn(
        &app,
        chat_turn::Model {
            last_progress_at: Some(
                now(&app) - Duration::seconds(300) + Duration::milliseconds(500),
            ),
            ..row
        },
    )
    .await;

    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(report, ScanReport::default());
    assert_eq!(turn(&app, row.id).await.state, "running");
}

#[tokio::test]
async fn null_progress_falls_back_to_started_at() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    let stale = insert_turn(
        &app,
        chat_turn::Model {
            last_progress_at: None,
            ..row.clone()
        },
    )
    .await;
    // A second chat whose NULL-progress turn started recently.
    let (_c2, row2) = chat_with_running_turn(&app).await;
    let fresh = insert_turn(
        &app,
        chat_turn::Model {
            last_progress_at: None,
            started_at: now(&app) - Duration::seconds(30),
            ..row2
        },
    )
    .await;

    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );
    let stale = turn(&app, stale.id).await;
    assert_eq!(stale.state, "failed");
    assert_eq!(stale.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(turn(&app, fresh.id).await.state, "running");
}

#[tokio::test]
async fn null_reserve_skips_settlement_but_enqueues_usage() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    // A retry/edit turn left running before its preflight columns were written.
    let row = insert_turn(
        &app,
        chat_turn::Model {
            reserve_tokens: None,
            max_output_tokens_applied: None,
            reserved_credits_micro: None,
            policy_version_applied: None,
            effective_model: None,
            minimal_generation_floor_applied: None,
            ..row
        },
    )
    .await;

    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );
    let done = turn(&app, row.id).await;
    assert_eq!(done.state, "failed");
    assert_eq!(done.error_code.as_deref(), Some("orphan_timeout"));
    assert!(quota_rows(&app).await.is_empty(), "no quota_usage change");

    let usage = app.outbox_payloads(USAGE_QUEUE).await;
    let usage = usage_for(&usage, row.request_id);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert!(u["usage"].is_null(), "{u}");
    assert_eq!(u["effective_model"], "");
    assert_eq!(u["selected_model"], "");
    assert_eq!(u["policy_version_applied"], 0);
}

#[tokio::test]
async fn cas_loser_noop() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    let candidate = insert_turn(&app, row).await;
    // Finalized by the stream path after the candidate was discovered.
    let conn = app.db.conn().unwrap();
    let won_by_stream = TurnRepo
        .finalize_cas(
            &conn,
            &tenant_scope(candidate.tenant_id, Uuid::nil()),
            candidate.id,
            &TurnTerminal {
                state: "completed",
                error_code: None,
                error_detail: None,
                assistant_message_id: None,
                provider_response_id: None,
                now: now(&app),
            },
        )
        .await
        .unwrap();
    assert_eq!(won_by_stream, 1);

    let won = watchdog(&app)
        .finalize_candidate(&candidate, now(&app))
        .await
        .unwrap();
    assert!(!won);
    let after = turn(&app, candidate.id).await;
    assert_eq!(after.state, "completed");
    assert_eq!(after.error_code, None);
    assert!(quota_rows(&app).await.is_empty());
    assert!(app.outbox_payloads(USAGE_QUEUE).await.is_empty());
    assert!(app.outbox_payloads(AUDIT_QUEUE).await.is_empty());
    // Not a candidate any more either.
    assert_eq!(
        watchdog(&app).scan_once(now(&app)).await,
        ScanReport::default()
    );
}

#[tokio::test]
async fn stuck_turn_unblocks_chat() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_live, _gate, _rid) = start_held_turn(&app, &client, chat).await;

    // While the turn runs, a new send is rejected.
    let busy = client
        .post_json(&stream_path(chat), &json!({"content": "again"}))
        .await;
    assert_eq!(busy.status, StatusCode::CONFLICT, "{}", busy.text());

    app.clock.advance(Duration::seconds(301));
    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(report.finalized, 1);

    push_hello(&app);
    let resp = client
        .post_json(&stream_path(chat), &json!({"content": "again"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let events = resp.sse_events();
    assert_eq!(names(&events).last().copied(), Some("done"), "{events:?}");
}

#[tokio::test]
async fn model_missing_from_snapshot_finalizes_without_settlement() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (_live, _gate, rid) = start_held_turn(&app, &client, chat).await;
    let running = turn_by_request(&app, rid).await;
    assert!(running.reserved_credits_micro.unwrap() > 0);

    // The operator removed `s1`: the turn's policy version no longer has it.
    app.policy.set_catalog(vec![premium_model("p1")]);
    app.clock.advance(Duration::seconds(301));
    let report = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );

    let done = turn_by_request(&app, rid).await;
    assert_eq!(done.state, "failed");
    assert_eq!(done.error_code.as_deref(), Some("orphan_timeout"));
    // The reserve is released without a debit (the model's price is unknown).
    for period in ["daily", "monthly"] {
        let row = quota_row_of(&app, period, "total").await;
        assert_eq!(row.reserved_credits_micro, 0, "{period}");
        assert_eq!(row.spent_credits_micro, 0, "{period}");
    }
    let usage = app.outbox_payloads(USAGE_QUEUE).await;
    let usage = usage_for(&usage, rid);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let u = usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["actual_credits_micro"], 0);
    assert!(u["usage"].is_null(), "{u}");
    assert_eq!(u["selected_model"], u["effective_model"]);

    // The chat is no longer blocked by a running turn.
    app.policy
        .set_catalog(vec![premium_model("p1"), standard_model("s1")]);
    push_hello(&app);
    let resp = client
        .post_json(&stream_path(chat), &json!({"content": "again"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
}

#[tokio::test]
async fn premium_reserve_released_when_model_missing() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    let (_live, _gate, rid) = start_held_turn(&app, &client, chat).await;
    let reserved = turn_by_request(&app, rid)
        .await
        .reserved_credits_micro
        .unwrap();
    assert_eq!(
        quota_row_of(&app, "daily", "tier:premium")
            .await
            .reserved_credits_micro,
        reserved
    );

    app.policy.set_catalog(vec![standard_model("s1")]);
    app.clock.advance(Duration::seconds(301));
    assert_eq!(watchdog(&app).scan_once(now(&app)).await.finalized, 1);
    // The user's only running turn: both the total and the premium rows
    // give the reserve back, without a debit.
    for period in ["daily", "monthly"] {
        for bucket in ["total", "tier:premium"] {
            let row = quota_row_of(&app, period, bucket).await;
            assert_eq!(row.reserved_credits_micro, 0, "{period}/{bucket}");
            assert_eq!(row.spent_credits_micro, 0, "{period}/{bucket}");
        }
    }
}

#[tokio::test]
async fn transient_policy_failure_is_retried() {
    let app = TestApp::builder().build().await;
    let (_c, row) = chat_with_running_turn(&app).await;
    let row = insert_turn(&app, row).await;
    app.policy
        .push_snapshot_error(DomainError::PluginUnavailable(
            "policy plugin down".to_owned(),
        ));

    let first = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(
        first,
        ScanReport {
            detected: 1,
            finalized: 0
        }
    );
    assert_eq!(turn(&app, row.id).await.state, "running");
    assert!(app.outbox_payloads(USAGE_QUEUE).await.is_empty());

    let second = watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(second.finalized, 1);
    assert_eq!(turn(&app, row.id).await.state, "failed");
}

// ---------------------------------------------------------------------------
// Upload reaper
// ---------------------------------------------------------------------------

/// A chat and an attachment row of it last updated long before the clock.
async fn chat_with_attachment(
    app: &TestApp,
    f: impl FnOnce(&mut attachment::Model),
) -> attachment::Model {
    let (user, tenant) = ids();
    let chat_id = create_chat(&app.as_user(user, tenant), "s1").await;
    let c = chat_model(app, tenant, user, chat_id).await;
    let mut row = attachment_row(&c);
    f(&mut row);
    let conn = app.db.conn().unwrap();
    AttachmentRepo
        .insert(&conn, &tenant_scope(tenant, user), row)
        .await
        .unwrap()
}

async fn attachment(app: &TestApp, id: Uuid) -> attachment::Model {
    attachment::Entity::find_by_id(id)
        .one(&app.raw)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn reaper_marks_abandoned_and_enqueues_cleanup() {
    let app = TestApp::builder().build().await;
    let row = chat_with_attachment(&app, |a| {
        a.status = "uploaded".to_owned();
        a.provider_file_id = Some("file-abandoned0001".to_owned());
    })
    .await;
    // Fresh `uploaded` row (inside stale_after): untouched.
    let fresh = chat_with_attachment(&app, |a| {
        a.status = "uploaded".to_owned();
        a.provider_file_id = Some("file-fresh000001".to_owned());
        a.updated_at = now(&app) - Duration::seconds(30);
    })
    .await;

    let report = reaper(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ReapReport {
            candidates: 1,
            abandoned: 1,
            cleanup_enqueued: 1
        }
    );

    let a = attachment(&app, row.id).await;
    assert_eq!(a.status, "failed");
    assert_eq!(a.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(a.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(a.deleted_at, None, "the row stays visible");
    assert_eq!(attachment(&app, fresh.id).await.status, "uploaded");

    let payloads = app.outbox_payloads(CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["event_type"], "attachment_upload_abandoned");
    assert_eq!(p["attachment_id"], row.id.to_string());
    assert_eq!(p["provider_file_id"], "file-abandoned0001");
    assert_eq!(p["chat_id"], row.chat_id.to_string());
}

#[tokio::test]
async fn reaper_pending_without_file_no_cleanup() {
    let app = TestApp::builder().build().await;
    let row = chat_with_attachment(&app, |a| a.status = "pending".to_owned()).await;

    let report = reaper(&app).scan_once(now(&app)).await;
    assert_eq!(
        report,
        ReapReport {
            candidates: 1,
            abandoned: 1,
            cleanup_enqueued: 0
        }
    );
    let a = attachment(&app, row.id).await;
    assert_eq!(a.status, "failed");
    assert_eq!(a.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(a.cleanup_status, None);
    assert!(app.outbox_payloads(CLEANUP_QUEUE).await.is_empty());
}

#[tokio::test]
async fn reaper_skips_rows_with_cleanup_status() {
    let app = TestApp::builder().build().await;
    // Claimed by chat cleanup (chat deleted).
    let claimed = chat_with_attachment(&app, |a| {
        a.status = "uploaded".to_owned();
        a.provider_file_id = Some("file-claimed00001".to_owned());
        a.cleanup_status = Some("pending".to_owned());
    })
    .await;
    // Soft-deleted rows and terminal rows are not candidates either.
    let deleted = chat_with_attachment(&app, |a| {
        a.status = "pending".to_owned();
        a.deleted_at = Some(ts(1_700_000_050));
    })
    .await;
    let ready = chat_with_attachment(&app, |a| {
        a.status = "ready".to_owned();
        a.provider_file_id = Some("file-ready0000001".to_owned());
    })
    .await;

    let report = reaper(&app).scan_once(now(&app)).await;
    assert_eq!(report, ReapReport::default());
    assert_eq!(attachment(&app, claimed.id).await.status, "uploaded");
    assert_eq!(attachment(&app, deleted.id).await.status, "pending");
    assert_eq!(attachment(&app, ready.id).await.status, "ready");
    assert!(app.outbox_payloads(CLEANUP_QUEUE).await.is_empty());
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_records_ttft_overhead() {
    let rec = MetricsRecorder::new();
    let app = TestApp::builder()
        .metrics(Arc::clone(&rec.metrics))
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);
    let resp = client
        .post_json(&stream_path(chat), &json!({"content": "hi"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK);

    let (count, labels) = rec.histogram("mini_chat_ttft_overhead_ms");
    assert_eq!(count, 1);
    assert!(
        labels.contains(&("provider".to_owned(), "openai".to_owned())),
        "{labels:?}"
    );
    assert!(
        labels.contains(&("model".to_owned(), "s1".to_owned())),
        "{labels:?}"
    );
}

#[tokio::test]
async fn background_upload_counts_as_pending_until_it_ends() {
    let rec = MetricsRecorder::new();
    let app = TestApp::builder()
        .metrics(Arc::clone(&rec.metrics))
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-late");
    script_vs_create(&app, "vs_l");
    script_add(&app, "vs_l", "file-late", "in_progress");
    status_fallback(&app, "vs_l", "file-late", "in_progress");
    let body = created(&upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await);
    assert_eq!(body["status"], "uploaded");
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    // Still processed by the background indexing task.
    assert_eq!(rec.up_down("mini_chat_attachments_pending"), 1);

    status_fallback(&app, "vs_l", "file-late", "completed");
    wait_until(5, || async {
        attachment_by_id(&app, id).await.status == "ready"
    })
    .await;
    wait_until(5, || async {
        rec.up_down("mini_chat_attachments_pending") == 0
    })
    .await;
    assert_eq!(rec.counter("mini_chat_attachment_background_indexing"), 1);
}

#[tokio::test]
async fn orphan_scan_records_metrics() {
    let rec = MetricsRecorder::new();
    let app = TestApp::builder()
        .metrics(Arc::clone(&rec.metrics))
        .build()
        .await;
    let (_c, row) = chat_with_running_turn(&app).await;
    insert_turn(&app, row).await;
    watchdog(&app).scan_once(now(&app)).await;
    assert_eq!(rec.counter("mini_chat_orphan_detected"), 1);
    assert_eq!(rec.counter("mini_chat_orphan_finalized"), 1);
    assert_eq!(rec.histogram("mini_chat_orphan_scan_duration_seconds").0, 1);
}
