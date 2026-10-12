#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Concurrent turn mutations resolve deterministically (DESIGN 3.9 rules 1 and 7, "Atomicity
//! invariant"): the latest-turn check, the terminal-state check, the soft delete of the old turn and
//! the insert of the new `running` turn are one transaction, so of two racing mutations of the same
//! turn exactly one wins; the loser gets `409 aborted` (`GENERATION_IN_PROGRESS` when it lost the
//! insert race on the one-running-turn index, `NOT_LATEST_TURN` when it saw the committed result), never
//! `500`, and the chat never has more than one running turn.

mod common;
mod live;
mod mutation;

use common::*;
use http::StatusCode;
use mini_chat::test_support::TestResponse;
use mutation::{
    Conversation, NEW_ANSWER, Op, all_messages, conversation, delete_turn, live_turns, mutate,
    mutation_audits, open_mutation, retry, running_turns,
};
use serde_json::json;
use uuid::Uuid;

/// How one racing call ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// `200` with an event stream (retry, edit).
    Streamed,
    /// `204` (delete).
    NoContent,
    /// `409 aborted` with this `context.reason`.
    Conflict(String),
}

fn classify(resp: &TestResponse) -> Outcome {
    match resp.status {
        StatusCode::OK => Outcome::Streamed,
        StatusCode::NO_CONTENT => Outcome::NoContent,
        StatusCode::CONFLICT => {
            let problem = assert_problem(resp, 409, Some("aborted"));
            Outcome::Conflict(
                problem["context"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
        other => panic!(
            "a racing mutation must end 200, 204 or 409, got {other}: {}",
            body_text(resp)
        ),
    }
}

fn is_race_loss(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Conflict(reason) if reason == "GENERATION_IN_PROGRESS" || reason == "NOT_LATEST_TURN"
    )
}

fn winners(outcomes: &[Outcome]) -> usize {
    outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Outcome::Streamed | Outcome::NoContent))
        .count()
}

async fn one_turn() -> Conversation {
    conversation(&[("Q-ONE", "A-ONE")]).await
}

fn queue_answers(c: &Conversation, count: usize) {
    for _ in 0..count {
        enqueue_text(&c.env, &[NEW_ANSWER], 10, 5);
    }
}

/// The invariants every outcome of a race on a chat with one turn must keep.
async fn assert_consistent(c: &Conversation, old: Uuid) {
    assert!(
        running_turns(&c.env, &c.chat).await.is_empty(),
        "no stuck running turn"
    );
    let turns = turn_rows(&c.env, &c.chat).await;
    let old_turn = turns.iter().find(|turn| turn.request_id == old).unwrap();
    assert!(
        old_turn.deleted,
        "the target was replaced or deleted exactly once"
    );
    let live = live_turns(&c.env, &c.chat).await;
    assert!(live.len() <= 1, "at most one replacement turn: {live:?}");
    assert_eq!(
        turns.len(),
        1 + live.len(),
        "no turn row beyond the winner's: {turns:?}"
    );
    assert_eq!(
        mutation_audits(&c.env).await.len(),
        1,
        "exactly one mutation was audited"
    );
    assert_eq!(total_reserved(&c.env).await, 0);
    let messages: Vec<_> = all_messages(&c.env, &c.chat).await;
    let live_messages = messages.iter().filter(|m| !m.deleted).count();
    assert_eq!(
        live_messages,
        2 * live.len(),
        "live messages are the winner's pair: {messages:?}"
    );
}

// ===========================================================================
// Two mutations of the same turn
// ===========================================================================

#[tokio::test]
async fn two_concurrent_retries_of_the_same_turn_start_exactly_one_new_turn() {
    for round in 0..5 {
        let c = one_turn().await;
        let old = c.first().request_id;
        queue_answers(&c, 2);

        let (a, b) = tokio::join!(
            retry(&c.env, &c.user, &c.chat, old),
            retry(&c.env, &c.user, &c.chat, old),
        );

        let outcomes = [classify(&a), classify(&b)];
        assert_eq!(winners(&outcomes), 1, "round {round}: {outcomes:?}");
        assert!(
            outcomes.iter().any(is_race_loss),
            "round {round}: the loser is 409 GENERATION_IN_PROGRESS or NOT_LATEST_TURN: {outcomes:?}"
        );
        assert_consistent(&c, old).await;
        assert_eq!(
            live_turns(&c.env, &c.chat).await.len(),
            1,
            "one new turn exists"
        );
        assert_eq!(
            c.env.provider().chat_requests().len(),
            2,
            "one provider call for the winner"
        );
    }
}

#[tokio::test]
async fn several_concurrent_retries_still_start_exactly_one_new_turn() {
    let c = one_turn().await;
    let old = c.first().request_id;
    queue_answers(&c, 4);

    let (first, second, third, fourth) = tokio::join!(
        retry(&c.env, &c.user, &c.chat, old),
        retry(&c.env, &c.user, &c.chat, old),
        retry(&c.env, &c.user, &c.chat, old),
        retry(&c.env, &c.user, &c.chat, old),
    );

    let outcomes = [
        classify(&first),
        classify(&second),
        classify(&third),
        classify(&fourth),
    ];
    assert_eq!(winners(&outcomes), 1, "{outcomes:?}");
    assert_eq!(
        outcomes.iter().filter(|o| is_race_loss(o)).count(),
        3,
        "{outcomes:?}"
    );
    assert_consistent(&c, old).await;
}

#[tokio::test]
async fn a_concurrent_retry_and_edit_of_the_same_turn_have_one_winner() {
    let c = one_turn().await;
    let old = c.first().request_id;
    queue_answers(&c, 2);

    let (a, b) = tokio::join!(
        mutate(&c.env, &c.user, &c.chat, old, Op::Retry),
        mutate(&c.env, &c.user, &c.chat, old, Op::Edit),
    );

    let outcomes = [classify(&a), classify(&b)];
    assert_eq!(winners(&outcomes), 1, "{outcomes:?}");
    assert!(outcomes.iter().any(is_race_loss), "{outcomes:?}");
    assert_consistent(&c, old).await;
}

#[tokio::test]
async fn a_concurrent_retry_and_delete_of_the_same_turn_have_one_winner() {
    let c = one_turn().await;
    let old = c.first().request_id;
    queue_answers(&c, 1);

    let (a, b) = tokio::join!(
        mutate(&c.env, &c.user, &c.chat, old, Op::Retry),
        mutate(&c.env, &c.user, &c.chat, old, Op::Delete),
    );

    let outcomes = [classify(&a), classify(&b)];
    assert_eq!(winners(&outcomes), 1, "{outcomes:?}");
    assert!(outcomes.iter().any(is_race_loss), "{outcomes:?}");
    let live = live_turns(&c.env, &c.chat).await.len();
    assert_eq!(
        live,
        usize::from(outcomes[0] == Outcome::Streamed),
        "a retry winner leaves its new turn, a delete winner leaves none: {outcomes:?}"
    );
    assert_consistent(&c, old).await;
}

#[tokio::test]
async fn two_concurrent_deletes_of_the_same_turn_have_one_winner() {
    let c = one_turn().await;
    let old = c.first().request_id;

    let (a, b) = tokio::join!(
        delete_turn(&c.env, &c.user, &c.chat, old),
        delete_turn(&c.env, &c.user, &c.chat, old),
    );

    let outcomes = [classify(&a), classify(&b)];
    assert_eq!(winners(&outcomes), 1, "{outcomes:?}");
    assert!(
        outcomes.contains(&Outcome::Conflict("NOT_LATEST_TURN".to_owned())),
        "the second delete finds the turn already deleted: {outcomes:?}"
    );
    assert_consistent(&c, old).await;
    assert!(live_turns(&c.env, &c.chat).await.is_empty());
}

// ===========================================================================
// The lost insert race, forced
// ===========================================================================

async fn check_a_lost_insert_race_is_409_generation_in_progress(op: Op) {
    let c = one_turn().await;
    let old = c.first().request_id;
    // Another request takes the running slot between the preview and the insert of the new turn:
    // the insert of a turn without reserve columns (the mutation commit) finds a running turn.
    exec_sql(
        &c.env,
        "CREATE TRIGGER lose_the_race BEFORE INSERT ON chat_turns WHEN NEW.reserve_tokens IS NULL \
         BEGIN INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
         requester_user_id, state, started_at, last_progress_at, updated_at) \
         VALUES (randomblob(16), NEW.tenant_id, NEW.chat_id, randomblob(16), 'user', \
         NEW.requester_user_id, 'running', NEW.started_at, NEW.started_at, NEW.started_at); END",
    )
    .await;
    let before = mutation::snapshot(&c.env, &c.chat).await;
    let calls = c.env.provider().chat_requests().len();
    mutation::prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, old, op).await;

    assert_aborted(&resp, "GENERATION_IN_PROGRESS");
    assert!(content_type(&resp).starts_with("application/problem+json"));
    // Everything the mutation did is rolled back with the failed insert: the old turn is neither
    // deleted nor replaced, no message was added, no audit event was enqueued.
    mutation::assert_untouched(&c.env, &c.chat, &before, calls).await;
}
mutation::per_replacing_op!(
    a_lost_insert_race_is_409_generation_in_progress_not_500,
    check_a_lost_insert_race_is_409_generation_in_progress
);

// ===========================================================================
// A mutation racing a new send, with running turns that stay open
// ===========================================================================

#[tokio::test]
async fn a_retry_and_a_send_racing_for_the_running_slot_leave_one_running_turn() {
    let c = one_turn().await;
    let old = c.first().request_id;
    enqueue_chat(&c.env, hang_script(&["par"]));
    enqueue_chat(&c.env, hang_script(&["par"]));

    let (retried, sent) = tokio::join!(
        open_mutation(&c.env, &c.user, &c.chat, old, Op::Retry),
        live::open(&c.env, &c.user, &c.chat, json!({ "content": "Q-RACE" })),
    );

    let statuses = [retried.status, sent.status];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "exactly one of the two takes the running slot: {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        1,
        "the other one is 409, never 500: {statuses:?}"
    );
    assert_eq!(
        running_turns(&c.env, &c.chat).await.len(),
        1,
        "at most one running turn"
    );
    retried.disconnect();
    sent.disconnect();
}

#[tokio::test]
async fn racing_retries_of_a_turn_whose_answer_is_still_streaming_never_run_two_generations() {
    let c = one_turn().await;
    let old = c.first().request_id;
    enqueue_chat(&c.env, hang_script(&["par"]));
    enqueue_chat(&c.env, hang_script(&["par"]));

    let (a, b) = tokio::join!(
        open_mutation(&c.env, &c.user, &c.chat, old, Op::Retry),
        open_mutation(&c.env, &c.user, &c.chat, old, Op::Retry),
    );

    let statuses = [a.status, b.status];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(running_turns(&c.env, &c.chat).await.len(), 1);
    assert_eq!(
        c.env.provider().chat_requests().len(),
        2,
        "the loser never reached the provider (one turn plus the winner's retry)"
    );
    a.disconnect();
    b.disconnect();
}

// ===========================================================================
// A mutation is serialized with a send on the chat row
// ===========================================================================

/// Logs, in execution order, the writes that decide the order of a mutation and a send: every
/// write of the chat row (`chat`), the soft delete of a turn (`turn_retired`) and every new turn
/// (`turn_insert`). The log is written inside the writing transactions, so a rolled-back
/// transaction leaves no entry.
async fn log_ordering_writes(env: &mini_chat::test_support::TestEnv) {
    for sql in [
        "CREATE TABLE write_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, what TEXT NOT NULL)",
        "CREATE TRIGGER log_chat_write AFTER UPDATE ON chats \
         BEGIN INSERT INTO write_log (what) VALUES ('chat'); END",
        "CREATE TRIGGER log_turn_retired AFTER UPDATE ON chat_turns \
         WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL \
         BEGIN INSERT INTO write_log (what) VALUES ('turn_retired'); END",
        "CREATE TRIGGER log_turn_insert AFTER INSERT ON chat_turns \
         BEGIN INSERT INTO write_log (what) VALUES ('turn_insert'); END",
    ] {
        exec_sql(env, sql).await;
    }
}

async fn ordering_log(env: &mini_chat::test_support::TestEnv) -> Vec<String> {
    let log = scalar_text(
        env,
        "SELECT COALESCE(group_concat(what, ','), '') AS t \
         FROM (SELECT what FROM write_log ORDER BY seq)",
    )
    .await;
    log.split(',')
        .filter(|what| !what.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn check_the_chat_row_is_written_before_the_latest_turn_check(op: Op) {
    let c = one_turn().await;
    let old = c.first().request_id;
    let chat_before = mutation::chat_updated_at(&c.env, &c.chat).await;
    log_ordering_writes(&c.env).await;
    mutation::prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, old, op).await;

    assert!(
        matches!(classify(&resp), Outcome::Streamed | Outcome::NoContent),
        "{}",
        body_text(&resp)
    );
    let log = ordering_log(&c.env).await;
    assert_eq!(
        log.first().map(String::as_str),
        Some("chat"),
        "the first write of the mutation transaction locks the chat row: {log:?}"
    );
    let retired = log.iter().position(|what| what == "turn_retired").unwrap();
    assert!(retired > 0, "{log:?}");
    if op == Op::Delete {
        assert_eq!(
            mutation::chat_updated_at(&c.env, &c.chat).await,
            chat_before,
            "deleting a turn takes the chat lock without bumping the chat's activity"
        );
    } else {
        assert!(mutation::chat_updated_at(&c.env, &c.chat).await > chat_before);
    }
}
mutation::per_op!(
    a_mutation_writes_the_chat_row_before_it_checks_the_latest_turn,
    check_the_chat_row_is_written_before_the_latest_turn_check
);

#[tokio::test]
async fn a_delete_racing_a_send_never_commits_after_the_newer_turn_it_did_not_see() {
    for round in 0..5 {
        let c = one_turn().await;
        let old = c.first().request_id;
        enqueue_text(&c.env, &[NEW_ANSWER], 10, 5);
        log_ordering_writes(&c.env).await;

        let (deleted, sent) = tokio::join!(
            delete_turn(&c.env, &c.user, &c.chat, old),
            send_text(&c.env, &c.user, &c.chat, "Q-RACE"),
        );

        let deleted = classify(&deleted);
        assert!(
            matches!(&deleted, Outcome::NoContent)
                || matches!(&deleted, Outcome::Conflict(reason) if reason == "NOT_LATEST_TURN"),
            "round {round}: {deleted:?}"
        );
        assert_eq!(sent.status, StatusCode::OK, "round {round}");
        let log = ordering_log(&c.env).await;
        if deleted == Outcome::NoContent {
            let retired = log.iter().position(|what| what == "turn_retired").unwrap();
            let inserted = log.iter().position(|what| what == "turn_insert").unwrap();
            assert!(
                retired < inserted,
                "round {round}: a committed delete saw no newer turn, so the send inserted its \
                 turn after it: {log:?}"
            );
        } else {
            assert!(
                !log.iter().any(|what| what == "turn_retired"),
                "round {round}: a refused delete leaves no soft delete: {log:?}"
            );
            assert!(
                !turn_rows(&c.env, &c.chat)
                    .await
                    .iter()
                    .any(|turn| turn.deleted),
                "round {round}"
            );
        }
        assert!(running_turns(&c.env, &c.chat).await.is_empty());
    }
}

// ===========================================================================
// Different chats do not interfere
// ===========================================================================

#[tokio::test]
async fn concurrent_retries_in_different_chats_both_succeed() {
    let env = env_two_models().await;
    let user = mini_chat::test_support::TestUser::new();
    let chat_a = new_chat(&env, &user).await;
    let chat_b = new_chat(&env, &user).await;
    let turn_a = complete_turn(&env, &user, &chat_a, "Q-A", "A-A").await;
    let turn_b = complete_turn(&env, &user, &chat_b, "Q-B", "A-B").await;
    enqueue_text(&env, &[NEW_ANSWER], 10, 5);
    enqueue_text(&env, &[NEW_ANSWER], 10, 5);

    let (a, b) = tokio::join!(
        retry(&env, &user, &chat_a, turn_a.request_id),
        retry(&env, &user, &chat_b, turn_b.request_id),
    );

    assert_eq!(classify(&a), Outcome::Streamed, "{}", body_text(&a));
    assert_eq!(classify(&b), Outcome::Streamed, "{}", body_text(&b));
    for chat in [&chat_a, &chat_b] {
        assert_eq!(live_turns(&env, chat).await.len(), 1);
        assert!(running_turns(&env, chat).await.is_empty());
    }
    assert_eq!(total_reserved(&env).await, 0);
}
