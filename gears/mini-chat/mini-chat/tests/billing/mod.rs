#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Helpers for the S3 tests that need the S3-only harness API (HARNESS.md "S3 additions"):
//! `run_orphan_watchdog_once`, `fail_next_usage_publishes`, `reject_next_usage_publishes`,
//! `usage_publish_attempts`, `enqueue_raw_outbox`, `outbox_outcomes` and
//! `TestEnvBuilder::without_audit_plugin`. They are kept apart from `common` and `live` so the S1 and
//! S2 test binaries compile without them.
//!
//! A test file that needs them declares `mod common; mod live; mod billing;`.

use mini_chat::test_support::{StreamingResponse, TestEnv, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::common::{exec_sql, new_chat, ts_literal, uuid_hex};
use crate::live;

/// The instant the aged turns of the watchdog tests started at.
pub const T0: &str = "2026-01-01T00:00:00Z";

/// `T0 + secs`.
#[must_use]
pub fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::parse(T0, &Rfc3339).unwrap() + Duration::seconds(secs)
}

/// Rewrites the progress timestamps of every turn: `started_at` and `last_progress_at` (`NULL` when
/// `None`).
///
/// A turn that started at `started` booked its reserve in the quota rows of that day and month
/// (DESIGN "Quota period boundary invariant": the watchdog settles the periods derived from
/// `started_at`), so the quota rows are moved to the periods of `started` as well.
pub async fn age_turns(env: &TestEnv, started: OffsetDateTime, progress: Option<OffsetDateTime>) {
    let progress = progress.map_or_else(|| "NULL".to_owned(), ts_literal);
    exec_sql(
        env,
        &format!(
            "UPDATE chat_turns SET started_at = {}, last_progress_at = {progress}",
            ts_literal(started)
        ),
    )
    .await;
    let day = started.date();
    let month = day.replace_day(1).unwrap();
    exec_sql(
        env,
        &format!("UPDATE quota_usage SET period_start = '{day}' WHERE period_type = 'daily'"),
    )
    .await;
    exec_sql(
        env,
        &format!("UPDATE quota_usage SET period_start = '{month}' WHERE period_type = 'monthly'"),
    )
    .await;
}

/// The state (`running`, `failed`, ...) and error code of the only turn of `chat`.
pub async fn turn_state_of(
    env: &TestEnv,
    chat: &str,
    request_id: Uuid,
) -> (String, Option<String>) {
    let turn = crate::common::turn_row(env, chat, request_id)
        .await
        .expect("the turn row exists");
    (turn.state, turn.error_code)
}

/// A running turn whose provider stream stays open (the turn of a pod that never answers).
pub struct RunningTurn {
    pub chat: String,
    pub request_id: Uuid,
    pub stream: StreamingResponse,
}

/// Starts a hung turn of `user` in a fresh chat of `env`.
pub async fn running_turn(env: &TestEnv, user: &TestUser, content: &str) -> RunningTurn {
    let chat = new_chat(env, user).await;
    running_turn_in(env, user, &chat, content).await
}

pub async fn running_turn_in(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    content: &str,
) -> RunningTurn {
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(env, user, chat, request_id, content, &["par"]).await;
    RunningTurn {
        chat: chat.to_owned(),
        request_id,
        stream,
    }
}

/// `completed_at` of the turn as an instant.
pub async fn completed_at(env: &TestEnv, chat: &str, request_id: Uuid) -> OffsetDateTime {
    let text = crate::common::scalar_text(
        env,
        &format!(
            "SELECT CAST(completed_at AS TEXT) AS t FROM chat_turns \
             WHERE hex(chat_id) = '{}' AND hex(request_id) = '{}'",
            uuid_hex(Uuid::parse_str(chat).unwrap()),
            uuid_hex(request_id)
        ),
    )
    .await;
    OffsetDateTime::parse(&text, &Rfc3339).unwrap_or_else(|err| panic!("`{text}`: {err}"))
}

// ---------------------------------------------------------------------------
// Outbox handler outcomes and dead letters
// ---------------------------------------------------------------------------

/// Waits (at most 5 s) until `queue` has at least `count` handler outcomes and returns them all, in
/// order (`"ok"`, `"retry"`, `"reject"`).
pub async fn wait_for_outcomes(env: &TestEnv, queue: &str, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let outcomes = env.outbox_outcomes(queue).await;
        if outcomes.len() >= count {
            return outcomes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queue `{queue}` has {outcomes:?}, expected {count} handler outcomes"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A row of the outbox dead-letter store.
#[derive(Debug, Clone)]
pub struct DeadLetter {
    pub payload: Vec<u8>,
    pub last_error: Option<String>,
    pub status: String,
}

/// The dead letters of `queue`, oldest first.
pub async fn dead_letters(env: &TestEnv, queue: &str) -> Vec<DeadLetter> {
    let sql = format!(
        "SELECT d.payload AS payload, d.last_error AS last_error, d.status AS status \
         FROM mini_chat__outbox_dead_letters d \
         JOIN mini_chat__outbox_partitions p ON p.id = d.partition_id \
         WHERE p.queue = '{queue}' ORDER BY d.id"
    );
    env.db()
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("the dead letter query succeeds")
        .iter()
        .map(|row| DeadLetter {
            payload: row.try_get("", "payload").unwrap(),
            last_error: row.try_get("", "last_error").unwrap(),
            status: row.try_get("", "status").unwrap(),
        })
        .collect()
}
