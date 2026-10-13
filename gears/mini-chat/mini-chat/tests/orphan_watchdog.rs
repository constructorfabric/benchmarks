#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The orphan turn watchdog (DESIGN "Turn Lifecycle, Crash Recovery and Orphan Handling", B.9.1,
//! 5.4.4, 5.7 `FinalizeTurn` Invariant, 5.8): a `running` turn whose `last_progress_at` (or `started_at`
//! when NULL) is older than `orphan_watchdog.timeout_secs` is finalized as `failed` /
//! `orphan_timeout` with an estimated settlement, a usage event (`aborted` / `estimated`) and an audit
//! event, all behind a CAS that re-checks `state = 'running'`, `deleted_at IS NULL` and the stale
//! progress.
//!
//! The scan is driven with `TestEnv::run_orphan_watchdog_once(now)`. A turn is aged with SQL to a
//! fixed `T0` (2026-01-01T00:00:00Z) and scanned at `T0 + secs`; the default timeout is 300 s. The
//! provider of a "crashed" turn stays open, which is what a turn left behind by a dead pod looks like
//! from the database.

mod billing;
mod common;
mod live;

use billing::{age_turns, at, completed_at, running_turn, running_turn_in, turn_state_of};
use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, USAGE_QUEUE, complete_turn, credits_micro,
    dedupe_key, env_user_chat, env_with_policy, estimated_credits, exec_sql, expect_quota_row,
    expect_stream, get, merge, message_rows, new_chat, only_turn, policy, priced_models,
    quota_rows, scalar_i64, send_text, total_reserved, ts_literal, turn_path,
};
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use mini_chat_sdk::ModelPolicyPluginError;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// The default `orphan_watchdog.timeout_secs`.
const TIMEOUT: i64 = 300;

/// A premium turn that stopped making progress at `T0`.
async fn stale_turn() -> (TestEnv, TestUser, billing::RunningTurn) {
    let (env, user, _chat) = env_user_chat().await;
    let turn = running_turn(&env, &user, "hello").await;
    age_turns(&env, at(0), Some(at(0))).await;
    (env, user, turn)
}

async fn state(env: &TestEnv, turn: &billing::RunningTurn) -> (String, Option<String>) {
    turn_state_of(env, &turn.chat, turn.request_id).await
}

// ===========================================================================
// Which turns are orphans
// ===========================================================================

#[tokio::test]
async fn a_turn_without_progress_for_longer_than_the_timeout_is_failed_with_orphan_timeout() {
    let (env, _user, turn) = stale_turn().await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let row = common::turn_row(&env, &turn.chat, turn.request_id)
        .await
        .unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("orphan_timeout"));
    assert!(row.has_completed_at);
    assert!(!row.deleted);
    assert!(row.assistant_message_id.is_none());
}

#[tokio::test]
async fn completed_at_is_the_application_time_of_the_scan() {
    let (env, _user, turn) = stale_turn().await;
    let now = at(TIMEOUT + 1);

    env.run_orphan_watchdog_once(now).await;

    assert_eq!(completed_at(&env, &turn.chat, turn.request_id).await, now);
}

#[tokio::test]
async fn the_stale_progress_cutoff_is_inclusive() {
    let (env, _user, turn) = stale_turn().await;

    env.run_orphan_watchdog_once(at(TIMEOUT - 1)).await;
    assert_eq!(
        state(&env, &turn).await.0,
        "running",
        "299 s without progress"
    );

    env.run_orphan_watchdog_once(at(TIMEOUT)).await;
    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "last_progress_at <= now - timeout"
    );
}

#[tokio::test]
async fn a_turn_that_started_long_ago_but_progressed_recently_is_not_finalized() {
    let (env, _user, turn) = stale_turn().await;
    age_turns(&env, at(0), Some(at(540))).await;

    env.run_orphan_watchdog_once(at(600)).await;

    assert_eq!(
        state(&env, &turn).await.0,
        "running",
        "progress at 540 s is newer than the cutoff 300 s even though the turn started at 0 s"
    );
    assert!(env.outbox_payloads(USAGE_QUEUE).await.is_empty());

    env.run_orphan_watchdog_once(at(540 + TIMEOUT + 60)).await;

    assert_eq!(
        state(&env, &turn).await.0,
        "failed",
        "stale once the progress is old too"
    );
}

#[tokio::test]
async fn without_last_progress_at_the_start_time_decides() {
    let (env, _user, turn) = stale_turn().await;
    age_turns(&env, at(0), None).await;

    env.run_orphan_watchdog_once(at(TIMEOUT - 1)).await;
    assert_eq!(state(&env, &turn).await.0, "running");

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;
    assert_eq!(state(&env, &turn).await.0, "failed");
}

#[tokio::test]
async fn the_timeout_is_the_configured_orphan_watchdog_timeout() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "orphan_watchdog": { "timeout_secs": 90 } }))
        .build()
        .await;
    let user = TestUser::new();
    let turn = running_turn(&env, &user, "hello").await;
    age_turns(&env, at(0), Some(at(0))).await;

    env.run_orphan_watchdog_once(at(89)).await;
    assert_eq!(state(&env, &turn).await.0, "running");

    env.run_orphan_watchdog_once(at(91)).await;
    assert_eq!(state(&env, &turn).await.0, "failed");
}

#[tokio::test]
async fn a_soft_deleted_turn_is_skipped() {
    let (env, user, turn) = stale_turn().await;
    exec_sql(
        &env,
        "UPDATE chat_turns SET deleted_at = '2000-01-01T00:00:00Z'",
    )
    .await;
    let before = quota_rows(&env, &user).await;

    env.run_orphan_watchdog_once(at(3600)).await;

    assert_eq!(state(&env, &turn).await.0, "running");
    assert!(env.outbox_payloads(USAGE_QUEUE).await.is_empty());
    assert!(env.outbox_payloads(AUDIT_QUEUE).await.is_empty());
    assert_eq!(quota_rows(&env, &user).await, before, "no settlement");
}

#[tokio::test]
async fn terminal_turns_are_never_touched() {
    let (env, user, chat) = env_user_chat().await;
    let completed = complete_turn(&env, &user, &chat, "hello", "answer").await;
    let cancelled = running_turn_in(&env, &user, &chat, "again").await;
    live::disconnect_and_wait_cancelled(&env, &chat, cancelled.request_id, cancelled.stream).await;
    age_turns(&env, at(0), Some(at(0))).await;
    let rows = quota_rows(&env, &user).await;
    let usage = env.outbox_payloads(USAGE_QUEUE).await.len();

    env.run_orphan_watchdog_once(at(3600)).await;

    assert_eq!(
        turn_state_of(&env, &chat, completed.request_id).await,
        ("completed".to_owned(), None)
    );
    assert_eq!(
        turn_state_of(&env, &chat, cancelled.request_id).await.0,
        "cancelled"
    );
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        usage,
        "no second event"
    );
    assert_eq!(quota_rows(&env, &user).await, rows, "no second settlement");
}

#[tokio::test]
async fn an_orphan_of_another_tenant_is_finalized_by_the_same_scan() {
    let env = env_with_policy(policy(common::two_model_catalog())).await;
    let alice = TestUser::new();
    let bob = TestUser::new();
    let alice_turn = running_turn(&env, &alice, "hello").await;
    let bob_turn = running_turn(&env, &bob, "hello").await;
    age_turns(&env, at(0), Some(at(0))).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(state(&env, &alice_turn).await.0, "failed");
    assert_eq!(state(&env, &bob_turn).await.0, "failed");
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 2, "{usage:?}");
    for user in [&alice, &bob] {
        assert!(
            usage.iter().any(
                |event| event["tenant_id"] == json!(user.tenant_id.to_string())
                    && event["user_id"] == json!(user.user_id.to_string())
            ),
            "{usage:?}"
        );
    }
}

#[tokio::test]
async fn a_scan_picks_up_at_most_one_hundred_candidates_and_the_next_scan_the_rest() {
    let (env, user, _chat) = env_user_chat().await;
    for _ in 0..101 {
        let chat = Uuid::new_v4();
        exec_sql(
            &env,
            &format!(
                "INSERT INTO chats (id, tenant_id, user_id, model, title, is_temporary, created_at, \
                 updated_at) VALUES ({}, {}, {}, '{PREMIUM_MODEL}', NULL, 0, {ts}, {ts})",
                common::blob(chat),
                common::blob(user.tenant_id),
                common::blob(user.user_id),
                ts = ts_literal(at(0)),
            ),
        )
        .await;
        exec_sql(
            &env,
            &format!(
                "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
                 requester_user_id, state, started_at, last_progress_at, updated_at) VALUES \
                 ({}, {}, {}, {}, 'user', {}, 'running', {ts}, {ts}, {ts})",
                common::blob(Uuid::new_v4()),
                common::blob(user.tenant_id),
                common::blob(chat),
                common::blob(Uuid::new_v4()),
                common::blob(user.user_id),
                ts = ts_literal(at(0)),
            ),
        )
        .await;
    }
    let running = || {
        scalar_i64(
            &env,
            "SELECT COUNT(*) AS n FROM chat_turns WHERE state = 'running'",
        )
    };

    env.run_orphan_watchdog_once(at(3600)).await;

    assert_eq!(running().await, 1, "100 candidates per scan");
    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), 100);

    env.run_orphan_watchdog_once(at(3600)).await;

    assert_eq!(running().await, 0);
    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), 101);
}

// ===========================================================================
// Idempotency and the CAS
// ===========================================================================

#[tokio::test]
async fn a_second_scan_changes_nothing() {
    let (env, user, turn) = stale_turn().await;
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;
    let rows = quota_rows(&env, &user).await;
    let completed = completed_at(&env, &turn.chat, turn.request_id).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;
    env.run_orphan_watchdog_once(at(7200)).await;

    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        1,
        "one usage event per turn"
    );
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
    assert_eq!(quota_rows(&env, &user).await, rows, "settled once");
    assert_eq!(
        completed_at(&env, &turn.chat, turn.request_id).await,
        completed,
        "a terminal turn is immutable"
    );
}

#[tokio::test]
async fn a_late_disconnect_does_not_reopen_or_settle_the_orphaned_turn_again() {
    let (env, user, turn) = stale_turn().await;
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;
    let rows = quota_rows(&env, &user).await;

    turn.stream.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        turn_state_of(&env, &turn.chat, turn.request_id).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "the CAS loser leaves the winner's outcome"
    );
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        1,
        "exactly one usage event"
    );
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
    assert_eq!(quota_rows(&env, &user).await, rows);
    env.drain_outbox().await;
    assert_eq!(env.published_usage().len(), 1);
}

#[tokio::test]
async fn a_turn_finalized_by_the_stream_is_not_an_orphan_candidate_any_more() {
    let (env, user, chat) = env_user_chat().await;
    let completed = complete_turn(&env, &user, &chat, "hello", "answer").await;
    age_turns(&env, at(0), Some(at(0))).await;
    env.drain_outbox().await;

    env.run_orphan_watchdog_once(at(3600)).await;
    env.drain_outbox().await;

    assert_eq!(
        turn_state_of(&env, &chat, completed.request_id).await.0,
        "completed"
    );
    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(env.delivered_audit().len(), 1);
}

// ===========================================================================
// Settlement
// ===========================================================================

#[tokio::test]
async fn the_reserve_is_released_and_the_estimated_charge_is_spent_in_every_bucket() {
    let (env, user, turn) = stale_turn().await;
    let reserved = total_reserved(&env).await;
    assert!(reserved > 0, "the hung turn holds a reserve");

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let row = common::turn_row(&env, &turn.chat, turn.request_id)
        .await
        .unwrap();
    let charged = estimated_credits(&only_turn(&env, &turn.chat).await, 1_000_000, 3_000_000);
    assert!(charged < row.reserved_credits_micro.unwrap());
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let bucket_row = expect_quota_row(&rows, period, bucket);
        assert_eq!(bucket_row.spent, charged, "{period}/{bucket}");
        assert_eq!(bucket_row.reserved, 0, "{period}/{bucket}");
        assert_eq!(bucket_row.calls, 1, "{period}/{bucket}");
        assert_eq!(
            (bucket_row.input_tokens, bucket_row.output_tokens),
            (0, 0),
            "{period}/{bucket}: no usage is known"
        );
    }
    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn a_downgraded_turn_is_settled_with_the_standard_model_in_the_total_bucket_only() {
    let env = env_with_policy(merge(
        policy(priced_models(
            (1_000_000, 3_000_000),
            (2_000_000, 4_000_000),
        )),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let turn = running_turn(&env, &user, "hello").await;
    age_turns(&env, at(0), Some(at(0))).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let charged = estimated_credits(&only_turn(&env, &turn.chat).await, 2_000_000, 4_000_000);
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(
            (total.spent, total.reserved, total.calls),
            (charged, 0, 1),
            "{period}"
        );
        assert!(
            common::quota_row(&rows, period, "tier:premium")
                .is_none_or(|row| row.spent == 0 && row.reserved == 0),
            "{period}"
        );
    }
    assert_eq!(
        charged,
        credits_micro(113, 50, 2_000_000, 4_000_000),
        "estimated input 113 plus the floor 50"
    );
}

#[tokio::test]
async fn settlement_targets_the_periods_of_started_at_not_of_the_scan() {
    // A turn that started at 23:59 on the last day of a month and is found by a scan after midnight
    // must release its reserve in the rows of that day and that month.
    let (env, user, turn) = stale_turn().await;
    exec_sql(
        &env,
        "UPDATE quota_usage SET period_start = '2025-12-31' WHERE period_type = 'daily'",
    )
    .await;
    exec_sql(
        &env,
        "UPDATE quota_usage SET period_start = '2025-12-01' WHERE period_type = 'monthly'",
    )
    .await;
    let started = OffsetDateTime::parse("2025-12-31T23:59:00Z", &Rfc3339).unwrap();
    age_turns(&env, started, Some(started)).await;

    env.run_orphan_watchdog_once(at(1800)).await; // 2026-01-01T00:30:00Z

    let charged = estimated_credits(&only_turn(&env, &turn.chat).await, 1_000_000, 3_000_000);
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        rows.len(),
        4,
        "no row is created for the scan's day or month: {rows:?}"
    );
    for row in &rows {
        let expected_start = if row.period_type == "daily" {
            "2025-12-31"
        } else {
            "2025-12-01"
        };
        assert_eq!(row.period_start, expected_start, "{row:?}");
        assert_eq!(row.reserved, 0, "{row:?}");
        assert_eq!(row.spent, charged, "{row:?}");
    }
}

#[tokio::test]
async fn the_web_search_surcharge_stays_in_the_persisted_estimate() {
    let (env, user, _chat) = env_user_chat().await;
    let chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    common::enqueue_chat(&env, common::hang_script(&["par"]));
    let mut stream = live::open(
        &env,
        &user,
        &chat,
        json!({
            "content": "hello", "request_id": request_id.to_string(),
            "web_search": { "enabled": true }
        }),
    )
    .await;
    live::read_until(&mut stream, "delta").await;
    age_turns(&env, at(0), Some(at(0))).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let turn = common::turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(
        turn.reserve_tokens.unwrap() - turn.max_output_tokens_applied.unwrap(),
        113 + 500,
        "estimated input includes the web search surcharge"
    );
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        estimated_credits(&turn, 1_000_000, 3_000_000)
    );
}

// ===========================================================================
// Events
// ===========================================================================

#[tokio::test]
async fn the_usage_event_is_aborted_and_estimated() {
    let (env, user, turn) = stale_turn().await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let row = only_turn(&env, &turn.chat).await;
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = &usage[0];
    assert_eq!(event["billing_outcome"], json!("aborted"));
    assert_eq!(event["settlement_method"], json!("estimated"));
    assert_eq!(event["terminal_state"], json!("failed"));
    assert_eq!(event["usage"], Value::Null);
    assert_eq!(
        event["actual_credits_micro"],
        json!(estimated_credits(&row, 1_000_000, 3_000_000))
    );
    assert_eq!(event["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["effective_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["policy_version_applied"], json!(1));
    assert_eq!(event["requester_type"], json!("user"));
    assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
    assert_eq!(event["user_id"], json!(user.user_id.to_string()));
    assert_eq!(event["chat_id"], json!(turn.chat));
    assert_eq!(event["request_id"], json!(turn.request_id.to_string()));
    assert_eq!(event["turn_id"], json!(row.id.to_string()));
    assert_eq!(
        event["dedupe_key"],
        json!(dedupe_key(user.tenant_id, row.id, turn.request_id))
    );
    for counter in [
        "web_search_calls",
        "code_interpreter_calls",
        "file_search_calls",
    ] {
        assert_eq!(event[counter], json!(0), "{counter}");
    }
}

#[tokio::test]
async fn the_selected_model_of_an_orphan_event_is_the_effective_model() {
    let env = env_with_policy(merge(
        policy(common::two_model_catalog()),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let _turn = running_turn(&env, &user, "hello").await;
    age_turns(&env, at(0), Some(at(0))).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(
        usage["selected_model"],
        json!(STANDARD_MODEL),
        "the watchdog records selected_model = the effective model"
    );
    let audit = &env.outbox_payloads(AUDIT_QUEUE).await[0];
    assert_eq!(audit["selected_model"], json!(STANDARD_MODEL));
    assert_eq!(audit["effective_model"], json!(STANDARD_MODEL));
}

#[tokio::test]
async fn the_audit_event_is_turn_failed_with_an_unknown_quota_decision() {
    let (env, user, turn) = stale_turn().await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let audit = env.outbox_payloads(AUDIT_QUEUE).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    let event = &audit[0];
    assert_eq!(event["event_type"], json!("turn_failed"));
    assert_eq!(event["terminal_state"], json!("failed"));
    assert_eq!(event["error_code"], json!("orphan_timeout"));
    assert_eq!(
        event["policy_decisions"]["quota"]["decision"],
        json!("unknown")
    );
    assert!(
        event["policy_decisions"]["quota"]
            .get("downgrade_from")
            .is_none(),
        "{event}"
    );
    assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
    assert_eq!(event["chat_id"], json!(turn.chat));
    assert_eq!(event["request_id"], json!(turn.request_id.to_string()));
    assert_eq!(event["prompt"], json!(""));
    assert_eq!(event["response"], json!(""));
}

#[tokio::test]
async fn the_events_reach_the_plugins_once() {
    let (env, _user, _turn) = stale_turn().await;
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    env.drain_outbox().await;
    env.drain_outbox().await;

    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(env.delivered_audit().len(), 1);
    assert_eq!(
        env.published_usage()[0]["billing_outcome"],
        json!("aborted")
    );
}

#[tokio::test]
async fn the_watchdog_neither_creates_nor_changes_messages() {
    let (env, _user, turn) = stale_turn().await;
    let before = message_rows(&env, &turn.chat).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let after = message_rows(&env, &turn.chat).await;
    assert_eq!(after.len(), before.len());
    assert_eq!(after.len(), 1, "only the persisted user message");
    assert_eq!(after[0].role, "user");
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].content, before[0].content);
}

// ===========================================================================
// After the finalization
// ===========================================================================

#[tokio::test]
async fn the_turn_status_shows_an_error_with_orphan_timeout() {
    let (env, user, turn) = stale_turn().await;
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let resp = get(
        &env,
        &user,
        &turn_path(&turn.chat, &turn.request_id.to_string()),
    )
    .await;

    common::expect_status(&resp, http::StatusCode::OK);
    let status = resp.json();
    assert_eq!(status["state"], json!("error"));
    assert_eq!(status["error_code"], json!("orphan_timeout"));
    assert!(status.get("assistant_message_id").is_none(), "{status}");
}

#[tokio::test]
async fn the_chat_accepts_a_new_turn_after_the_orphan_is_finalized() {
    let (env, user, turn) = stale_turn().await;
    // While the orphan runs the chat is busy.
    let busy = send_text(&env, &user, &turn.chat, "second").await;
    common::assert_aborted(&busy, "turn_already_running");
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let events = expect_stream(&send_text(&env, &user, &turn.chat, "second").await);

    assert_eq!(common::terminal(&events).event, "done", "{events:?}");
    assert_eq!(common::turn_rows(&env, &turn.chat).await.len(), 2);
}

#[tokio::test]
async fn the_orphaned_user_message_stays_in_the_conversation() {
    let (env, user, turn) = stale_turn().await;
    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let page = common::list_messages_ok(&env, &user, &turn.chat, &[]).await;

    assert_eq!(common::message_roles(&page), ["user"]);
    assert_eq!(common::message_contents(&page), ["hello"]);
}

// ===========================================================================
// A turn without reserve fields (a retry/edit turn left running before its reserve was written)
// ===========================================================================

#[tokio::test]
async fn a_turn_without_reserve_fields_is_finalized_without_any_quota_change() {
    let (env, user, turn) = stale_turn().await;
    exec_sql(
        &env,
        "UPDATE chat_turns SET reserve_tokens = NULL, max_output_tokens_applied = NULL, \
         reserved_credits_micro = NULL, policy_version_applied = NULL, effective_model = NULL, \
         minimal_generation_floor_applied = NULL",
    )
    .await;
    let before = quota_rows(&env, &user).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned()))
    );
    assert_eq!(
        quota_rows(&env, &user).await,
        before,
        "no quota_usage row changes"
    );
}

#[tokio::test]
async fn a_turn_without_reserve_fields_still_emits_a_zero_usage_event() {
    let (env, user, turn) = stale_turn().await;
    exec_sql(
        &env,
        "UPDATE chat_turns SET reserve_tokens = NULL, max_output_tokens_applied = NULL, \
         reserved_credits_micro = NULL, policy_version_applied = NULL, effective_model = NULL, \
         minimal_generation_floor_applied = NULL",
    )
    .await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = &usage[0];
    assert_eq!(event["billing_outcome"], json!("aborted"));
    assert_eq!(event["settlement_method"], json!("estimated"));
    assert_eq!(event["actual_credits_micro"], json!(0));
    assert_eq!(event["usage"], Value::Null);
    assert_eq!(event["effective_model"], json!(""));
    assert_eq!(event["selected_model"], json!(""));
    assert_eq!(event["policy_version_applied"], json!(0));
    assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
    assert_eq!(event["request_id"], json!(turn.request_id.to_string()));
}

// ===========================================================================
// A settlement basis that can never be resolved must not leave the turn running
// ===========================================================================

/// Inserts a `running` turn of `user` (own chat) that stopped progressing `secs` after `T0`.
/// `reserve` writes the six reserve columns of a premium turn; `requester` is the stored
/// `requester_user_id`.
async fn insert_running_turn(
    env: &TestEnv,
    user: &TestUser,
    secs: i64,
    reserve: bool,
    requester: Option<Uuid>,
) -> Uuid {
    let chat = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let ts = ts_literal(at(secs));
    exec_sql(
        env,
        &format!(
            "INSERT INTO chats (id, tenant_id, user_id, model, title, is_temporary, created_at, \
             updated_at) VALUES ({}, {}, {}, '{PREMIUM_MODEL}', NULL, 0, {ts}, {ts})",
            common::blob(chat),
            common::blob(user.tenant_id),
            common::blob(user.user_id),
        ),
    )
    .await;
    let (columns, values) = if reserve {
        (
            ", reserve_tokens, max_output_tokens_applied, reserved_credits_micro, \
             policy_version_applied, effective_model, minimal_generation_floor_applied",
            format!(", 4209, 4096, 12401, 1, '{PREMIUM_MODEL}', 50"),
        )
    } else {
        ("", String::new())
    };
    let requester = requester.map_or_else(|| "NULL".to_owned(), common::blob);
    exec_sql(
        env,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
             requester_user_id, state, started_at, last_progress_at, updated_at{columns}) VALUES \
             ({}, {}, {}, {}, 'user', {requester}, 'running', {ts}, {ts}, {ts}{values})",
            common::blob(turn),
            common::blob(user.tenant_id),
            common::blob(chat),
            common::blob(Uuid::new_v4()),
        ),
    )
    .await;
    turn
}

#[tokio::test]
async fn a_turn_whose_model_is_unknown_is_finalized_and_only_its_total_reserve_is_released() {
    let (env, user, turn) = stale_turn().await;
    let reserved = total_reserved(&env).await;
    assert!(reserved > 0, "the hung turn holds a reserve");
    let held = expect_quota_row(&quota_rows(&env, &user).await, "daily", "tier:premium").reserved;
    assert!(held > 0, "the premium turn holds a premium reserve");
    exec_sql(
        &env,
        "UPDATE chat_turns SET effective_model = 'retired-model'",
    )
    .await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "the turn is not left running"
    );
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(total.reserved, 0, "{period}/total");
        assert_eq!(total.spent, 0, "{period}/total: nothing is charged");
        let premium = expect_quota_row(&rows, period, "tier:premium");
        assert_eq!(
            premium.reserved, held,
            "{period}/tier:premium: the tier is unknown, the premium headroom stays held back"
        );
        assert_eq!(
            premium.spent, 0,
            "{period}/tier:premium: nothing is charged"
        );
    }
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["billing_outcome"], json!("aborted"));
    assert_eq!(usage[0]["settlement_method"], json!("released"));
    assert_eq!(usage[0]["actual_credits_micro"], json!(0));
    assert_eq!(usage[0]["effective_model"], json!("retired-model"));
    assert_eq!(usage[0]["user_id"], json!(user.user_id.to_string()));
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);

    let events = expect_stream(&send_text(&env, &user, &turn.chat, "second").await);

    assert_eq!(common::terminal(&events).event, "done", "{events:?}");
}

#[tokio::test]
async fn an_unknown_tier_orphan_does_not_release_the_premium_reserve_of_a_live_premium_turn() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let standard_chat = common::chat_on(&env, &user, STANDARD_MODEL).await;
    let premium_chat = new_chat(&env, &user).await;
    let orphan = running_turn_in(&env, &user, &standard_chat, "orphan").await;
    let live = running_turn_in(&env, &user, &premium_chat, "live").await;
    let live_reserve = common::only_turn(&env, &premium_chat)
        .await
        .reserved_credits_micro
        .unwrap();
    assert!(live_reserve > 0);
    // The orphan's model can no longer be resolved; the live turn made progress in the future of
    // the scan, so only the orphan is stale.
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    let scan = now + time::Duration::seconds(TIMEOUT + 1);
    exec_sql(
        &env,
        &format!(
            "UPDATE chat_turns SET effective_model = 'retired-model' WHERE chat_id = {}",
            common::blob(Uuid::parse_str(&standard_chat).unwrap())
        ),
    )
    .await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chat_turns SET last_progress_at = {} WHERE chat_id = {}",
            ts_literal(scan),
            common::blob(Uuid::parse_str(&premium_chat).unwrap())
        ),
    )
    .await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chat_turns SET last_progress_at = {} WHERE chat_id = {}",
            ts_literal(now),
            common::blob(Uuid::parse_str(&standard_chat).unwrap())
        ),
    )
    .await;
    let premium_before = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        assert_eq!(
            expect_quota_row(&premium_before, period, "tier:premium").reserved,
            live_reserve,
            "{period}: only the live premium turn holds premium reserve"
        );
    }

    env.run_orphan_watchdog_once(scan).await;

    assert_eq!(state(&env, &orphan).await.0, "failed");
    assert_eq!(state(&env, &live).await.0, "running");
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        assert_eq!(
            expect_quota_row(&rows, period, "total").reserved,
            live_reserve,
            "{period}/total: the orphan's reserve is released"
        );
        assert_eq!(
            expect_quota_row(&rows, period, "tier:premium").reserved,
            live_reserve,
            "{period}/tier:premium: the live turn's reserve is untouched"
        );
    }

    let billing::RunningTurn {
        chat,
        request_id,
        stream,
    } = live;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    assert_eq!(turn_state_of(&env, &chat, request_id).await.0, "cancelled");
    let rows = quota_rows(&env, &user).await;
    for row in &rows {
        assert!(row.reserved >= 0, "{row:?}");
        assert_eq!(row.reserved, 0, "{row:?}: every reserve is released");
    }
}

#[tokio::test]
async fn an_orphan_release_never_takes_a_reserve_below_zero() {
    let (env, user, turn) = stale_turn().await;
    exec_sql(
        &env,
        "UPDATE quota_usage SET reserved_credits_micro = 0 WHERE bucket = 'tier:premium'",
    )
    .await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "the clamped release does not fail the finalization"
    );
    for row in quota_rows(&env, &user).await {
        assert_eq!(row.reserved, 0, "{row:?}");
    }
}

#[tokio::test]
async fn an_orphan_whose_policy_version_lookup_fails_for_good_is_finalized_released() {
    let (env, user, turn) = stale_turn().await;
    env.fail_next_policy_snapshots([ModelPolicyPluginError::Internal("corrupt".to_owned())]);

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "a failure a retry will not fix is not retried"
    );
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["settlement_method"], json!("released"));
    assert_eq!(usage[0]["actual_credits_micro"], json!(0));
    let rows = quota_rows(&env, &user).await;
    for row in &rows {
        assert_eq!((row.reserved, row.spent), (0, 0), "{row:?}");
    }
    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn an_orphan_is_retried_while_the_policy_plugin_is_unavailable() {
    let (env, _user, turn) = stale_turn().await;
    env.fail_next_policy_snapshots([ModelPolicyPluginError::Unavailable("down".to_owned())]);

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await.0,
        "running",
        "retried by the next scan"
    );

    env.run_orphan_watchdog_once(at(TIMEOUT + 61)).await;

    assert_eq!(state(&env, &turn).await.0, "failed");
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage[0]["settlement_method"], json!("estimated"));
}

#[tokio::test]
async fn an_orphan_stale_past_the_retry_window_is_finalized_released_despite_a_plugin_outage() {
    let (env, user, turn) = stale_turn().await;
    let outage = || ModelPolicyPluginError::Unavailable("down".to_owned());
    env.fail_next_policy_snapshots([outage(), outage()]);

    // Stale for the cutoff plus three timeouts.
    env.run_orphan_watchdog_once(at(4 * TIMEOUT)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned())),
        "no turn stays running indefinitely"
    );
    // Both the recorded version and the current policy failed: the tier stays unknown, only the
    // total bucket is released and the premium bucket stays held back.
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        assert_eq!(
            expect_quota_row(&rows, period, "total").reserved,
            0,
            "{period}"
        );
        assert!(
            expect_quota_row(&rows, period, "tier:premium").reserved > 0,
            "{period}"
        );
    }
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage[0]["settlement_method"], json!("released"));
}

#[tokio::test]
async fn a_reserved_turn_without_a_requester_is_finalized_without_a_quota_change_or_a_user() {
    let (env, user, turn) = stale_turn().await;
    exec_sql(&env, "UPDATE chat_turns SET requester_user_id = NULL").await;
    let before = quota_rows(&env, &user).await;

    env.run_orphan_watchdog_once(at(TIMEOUT + 1)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned()))
    );
    assert_eq!(quota_rows(&env, &user).await, before, "no quota change");
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let user_id = usage[0].get("user_id").unwrap_or(&Value::Null);
    assert!(
        user_id.is_null(),
        "the usage event carries no user, not the nil id: {user_id}"
    );
    assert_eq!(usage[0]["billing_outcome"], json!("aborted"));
    assert_eq!(usage[0]["actual_credits_micro"], json!(0));
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn candidates_that_cannot_be_finalized_do_not_starve_the_turns_behind_them() {
    let (env, user, _chat) = env_user_chat().await;
    // 101 older turns whose reserve is booked on a user without quota rows: their finalization
    // transaction fails on every scan.
    for _ in 0..101 {
        insert_running_turn(&env, &user, 0, true, Some(Uuid::new_v4())).await;
    }
    let good = insert_running_turn(&env, &user, 10, false, Some(user.user_id)).await;

    env.run_orphan_watchdog_once(at(3600)).await;

    let good_state = scalar_text_of_turn(&env, good).await;
    assert_eq!(
        good_state, "failed",
        "the newer orphan is finalized in the same scan"
    );
}

async fn scalar_text_of_turn(env: &TestEnv, turn: Uuid) -> String {
    common::scalar_text(
        env,
        &format!(
            "SELECT state AS t FROM chat_turns WHERE id = {}",
            common::blob(turn)
        ),
    )
    .await
}

// ===========================================================================
// A reserve filled by a retry/edit turn while the watchdog decides
// ===========================================================================

/// Backs the six reserve columns of every turn up in a side table and clears them: the turn of a
/// retry/edit whose reserve step has not run yet.
async fn clear_reserve_keeping_a_backup(env: &TestEnv) {
    exec_sql(
        env,
        "CREATE TABLE reserve_backup AS SELECT id, reserve_tokens, max_output_tokens_applied, \
         reserved_credits_micro, policy_version_applied, effective_model, \
         minimal_generation_floor_applied FROM chat_turns",
    )
    .await;
    exec_sql(
        env,
        "UPDATE chat_turns SET reserve_tokens = NULL, max_output_tokens_applied = NULL, \
         reserved_credits_micro = NULL, policy_version_applied = NULL, effective_model = NULL, \
         minimal_generation_floor_applied = NULL",
    )
    .await;
}

/// What the mutation reserve commits: the six reserve columns, restored from the backup.
const FILL_RESERVE: &str = "UPDATE chat_turns SET \
    reserve_tokens = (SELECT reserve_tokens FROM reserve_backup b WHERE b.id = chat_turns.id), \
    max_output_tokens_applied = (SELECT max_output_tokens_applied FROM reserve_backup b WHERE b.id = chat_turns.id), \
    reserved_credits_micro = (SELECT reserved_credits_micro FROM reserve_backup b WHERE b.id = chat_turns.id), \
    policy_version_applied = (SELECT policy_version_applied FROM reserve_backup b WHERE b.id = chat_turns.id), \
    effective_model = (SELECT effective_model FROM reserve_backup b WHERE b.id = chat_turns.id), \
    minimal_generation_floor_applied = (SELECT minimal_generation_floor_applied FROM reserve_backup b WHERE b.id = chat_turns.id)";

#[tokio::test]
async fn a_reserve_filled_after_the_watchdog_read_makes_its_cas_lose_and_the_next_scan_settles_it()
{
    let (env, _user, turn) = stale_turn().await;
    let booked = total_reserved(&env).await;
    assert!(booked > 0, "the fixture turn holds a booked reserve");
    clear_reserve_keeping_a_backup(&env).await;

    // The watchdog reads NULL reserve fields (no settlement), then the mutation reserve commits.
    let (detected, finalized) = env
        .run_orphan_watchdog_once_racing(at(TIMEOUT + 1), FILL_RESERVE)
        .await;

    assert_eq!((detected, finalized), (1, 0), "the CAS lost");
    assert_eq!(
        state(&env, &turn).await.0,
        "running",
        "the turn is not finalized unsettled"
    );
    assert_eq!(total_reserved(&env).await, booked, "the reserve is intact");
    assert!(env.outbox_payloads(USAGE_QUEUE).await.is_empty());

    env.run_orphan_watchdog_once(at(TIMEOUT + 2)).await;

    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("orphan_timeout".to_owned()))
    );
    assert_eq!(
        total_reserved(&env).await,
        0,
        "the next scan re-reads the reserve and settles it"
    );
    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn fail_unstarted_never_finalizes_a_turn_that_holds_a_reserve() {
    let (env, user, turn) = stale_turn().await;
    let turn_id = common::turn_row(&env, &turn.chat, turn.request_id)
        .await
        .unwrap()
        .id;
    let booked = total_reserved(&env).await;

    let failed = env
        .fail_unstarted_turn(user.tenant_id, turn_id, "turn_setup_failed", at(1))
        .await;

    assert!(
        !failed,
        "a filled reserve is left to the settling finalizers"
    );
    assert_eq!(state(&env, &turn).await.0, "running");
    assert_eq!(total_reserved(&env).await, booked);

    // Without a reserve (the setup failed before the reserve step) the plain CAS applies.
    clear_reserve_keeping_a_backup(&env).await;
    let failed = env
        .fail_unstarted_turn(user.tenant_id, turn_id, "turn_setup_failed", at(1))
        .await;

    assert!(failed);
    assert_eq!(
        state(&env, &turn).await,
        ("failed".to_owned(), Some("turn_setup_failed".to_owned()))
    );
}
