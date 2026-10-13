#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The thread summary in the context of the next turn and its invalidation by turn mutations
//! (DESIGN section 3.4 "Context Plan Assembly and Truncation": thread summary delivery format,
//! recent messages query with `is_compressed` and the frontier predicate; section 3.9 "Summary
//! Interaction on Turn Mutation"; section 3.6 "Thread Summary Update").
//!
//! Summaries are seeded with SQL (`seed_summary`: the `thread_summaries` row plus
//! `is_compressed` on every message up to the frontier) unless a test wants the real trigger.

mod billing;
mod common;
mod live;
mod mutation;
mod summary;

use common::*;
use mini_chat::test_support::TestEnv;
use mutation::{
    Conversation, EDITED, NEW_ANSWER, Op, all_messages, assert_untouched, audit_types,
    conversation_in, delete_ok, expect_mutated, input_items, mutate, per_op, per_replacing_op,
    prepare, retry, snapshot, turn_detail,
};
use serde_json::{Value, json};
use summary::*;
use uuid::Uuid;

fn summary_message(text: &str) -> String {
    format!("{PREAMBLE}\n\n{text}")
}

fn items_of(body: &Value) -> Vec<(String, String)> {
    input_items(body)
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(role, text)| ((*role).to_owned(), (*text).to_owned()))
        .collect()
}

fn last_body(env: &TestEnv) -> Value {
    turn_requests(env)
        .pop()
        .expect("a chat request")
        .body_json
        .unwrap()
}

async fn three_turns() -> Conversation {
    quiet_conversation(&[
        ("Q-ONE", "A-ONE"),
        ("Q-TWO", "A-TWO"),
        ("Q-THREE", "A-THREE"),
    ])
    .await
}

// ===========================================================================
// The next turn uses the summary
// ===========================================================================

#[tokio::test]
async fn the_summary_is_a_user_role_message_with_the_preamble_followed_by_the_messages_after_the_frontier()
 {
    let c = three_turns().await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 3, 10).await;

    send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    let body = last_body(&c.env);
    assert_eq!(
        items_of(&body),
        pairs(&[
            ("user", &summary_message("SUMMARY-TEXT")),
            ("user", "Q-THREE"),
            ("assistant", "A-THREE"),
            ("user", "Q-NOW"),
        ]),
        "preamble and summary as one user message, then only the messages after the frontier"
    );
    assert!(
        !system_prompt_of(&body).contains("SUMMARY-TEXT"),
        "the summary is not part of the system prompt"
    );
    for covered in ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO"] {
        assert!(
            !input_text(&body).contains(covered),
            "`{covered}` is covered by the summary"
        );
    }
}

#[tokio::test]
async fn the_summary_text_is_sent_verbatim() {
    let c = three_turns().await;
    let text = "first line\n\nsecond line with \"quotes\" and \u{fc}ml\u{e4}uts \u{2014} done";
    seed_summary(&c.env, &c.user, &c.chat, text, 3, 10).await;

    send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    assert_eq!(items_of(&last_body(&c.env))[0].1, summary_message(text));
}

#[tokio::test]
async fn stream_started_announces_the_summary_and_its_estimated_tokens() {
    let c = three_turns().await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 3, 10).await;

    let events = send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    let expected =
        estimated_text_tokens(i64::try_from(summary_message("SUMMARY-TEXT").len()).unwrap());
    assert_eq!(
        started(&events).data["thread_summary_applied"],
        json!({ "token_estimate": expected }),
        "the preamble counts toward the estimated size of the summary"
    );
}

#[tokio::test]
async fn a_conversation_without_a_summary_has_the_plain_history_and_no_marker() {
    let c = three_turns().await;

    let events = send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    let texts: Vec<String> = items_of(&last_body(&c.env))
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        texts,
        [
            "Q-ONE", "A-ONE", "Q-TWO", "A-TWO", "Q-THREE", "A-THREE", "Q-NOW"
        ]
    );
    assert!(started(&events).data["thread_summary_applied"].is_null());
    assert!(!input_text(&last_body(&c.env)).contains("summarized"));
}

#[tokio::test]
async fn compressed_messages_are_excluded_whatever_their_position() {
    let c = three_turns().await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 1, 10).await;
    // A-TWO is flagged although it is after the frontier: is_compressed = false is required.
    // Q-ONE is unflagged although it is before the frontier: the frontier predicate excludes it.
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET is_compressed = 1 WHERE hex(id) = '{}'",
            uuid_hex(message_at(&c.env, &c.chat, 3).await)
        ),
    )
    .await;
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET is_compressed = 0 WHERE hex(id) = '{}'",
            uuid_hex(message_at(&c.env, &c.chat, 0).await)
        ),
    )
    .await;

    send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    assert_eq!(
        items_of(&last_body(&c.env)),
        pairs(&[
            ("user", &summary_message("SUMMARY-TEXT")),
            ("user", "Q-TWO"),
            ("user", "Q-THREE"),
            ("assistant", "A-THREE"),
            ("user", "Q-NOW"),
        ])
    );
}

#[tokio::test]
async fn the_recent_messages_limit_applies_after_the_frontier() {
    let env = Setup::new()
        .gear(json!({ "context": { "recent_messages_limit": 2 } }))
        .build()
        .await;
    let c = conversation_in(
        env,
        &[
            ("Q-ONE", "A-ONE"),
            ("Q-TWO", "A-TWO"),
            ("Q-THREE", "A-THREE"),
        ],
    )
    .await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 1, 10).await;

    send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    assert_eq!(
        items_of(&last_body(&c.env)),
        pairs(&[
            ("user", &summary_message("SUMMARY-TEXT")),
            ("user", "Q-THREE"),
            ("assistant", "A-THREE"),
            ("user", "Q-NOW"),
        ]),
        "the newest two messages after the frontier"
    );
}

#[tokio::test]
async fn compressed_messages_stay_in_the_conversation_listing() {
    let c = three_turns().await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 3, 10).await;

    let page = list_messages_ok(&c.env, &c.user, &c.chat, &[("limit", "100")]).await;

    assert_eq!(
        message_contents(&page),
        ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO", "Q-THREE", "A-THREE"],
        "is_compressed rows remain for the UI"
    );
}

#[tokio::test]
async fn a_summary_that_does_not_fit_the_budget_is_dropped_and_not_announced() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    // 12000 bytes: more than the whole 2900 token budget.
    seed_summary(&c.env, &c.user, &c.chat, &"S".repeat(12_000), 1, 3000).await;

    let events = send_ok(&c.env, &c.user, &c.chat, "Q-NOW").await;

    let body = last_body(&c.env);
    assert!(
        !input_text(&body).contains("summarized"),
        "dropped entirely: {body}"
    );
    assert!(started(&events).data["thread_summary_applied"].is_null());
    assert_eq!(items_of(&body).last().unwrap().1, "Q-NOW");
}

#[tokio::test]
async fn the_summary_of_one_chat_is_not_used_in_another_chat() {
    let c = three_turns().await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 3, 10).await;
    let other = new_chat(&c.env, &c.user).await;

    let events = send_ok(&c.env, &c.user, &other, "Q-OTHER").await;

    assert!(!input_text(&last_body(&c.env)).contains("SUMMARY-TEXT"));
    assert!(started(&events).data["thread_summary_applied"].is_null());
}

#[tokio::test]
async fn the_next_turn_after_a_triggered_summary_sees_the_stored_summary() {
    // The whole path: the third turn reaches the threshold (2770 of 3000 tokens), the handler
    // stores the summary, the fourth turn is built from it.
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, "SUMMARY-ONE");
    send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    enqueue_text(&c.env, &["A-FOUR"], 10, 5);

    let events = send_ok(&c.env, &c.user, &c.chat, "Q-FOUR").await;

    let items = items_of(&last_body(&c.env));
    assert_eq!(items.len(), 4, "{items:?}");
    assert_eq!(items[0].0, "user");
    assert!(items[0].1.starts_with(PREAMBLE), "{}", items[0].1);
    assert!(items[0].1.contains("SUMMARY-ONE"));
    assert_eq!(
        (items[1].0.as_str(), items[2].0.as_str()),
        ("user", "assistant")
    );
    assert!(
        items[1].1.starts_with("Q-THREE"),
        "the causing turn is not covered"
    );
    assert_eq!(items[2].1, "A-THREE");
    assert_eq!(items[3].1, "Q-FOUR");
    assert!(!started(&events).data["thread_summary_applied"].is_null());
    assert_eq!(
        tasks(&c.env).await.len(),
        1,
        "the summary is effective: no second task"
    );
    let page = list_messages_ok(&c.env, &c.user, &c.chat, &[("limit", "100")]).await;
    assert_eq!(items_len(&page), 8, "the covered messages stay listed");
}

fn items_len(page: &Value) -> usize {
    items(page).len()
}

// ===========================================================================
// A mutation drops a summary that covers the mutated turn
// ===========================================================================

/// Two turns; the summary frontier is the `frontier_index`-th message (0 = Q-ONE, 1 = A-ONE,
/// 2 = Q-TWO, 3 = A-TWO).
async fn summarized(frontier_index: usize) -> Conversation {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", frontier_index, 10).await;
    c
}

async fn assert_summary_kept(c: &Conversation) {
    let row = summary_row(&c.env, &c.chat)
        .await
        .expect("the summary is kept");
    assert_eq!(row.text, "SUMMARY-TEXT");
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 2).await,
        "the flags of the covered messages are kept"
    );
}

async fn assert_summary_dropped(c: &Conversation) {
    assert!(
        summary_row(&c.env, &c.chat).await.is_none(),
        "the thread_summaries row is deleted"
    );
    assert!(
        all_messages(&c.env, &c.chat)
            .await
            .iter()
            .all(|message| !message.compressed),
        "is_compressed is cleared on all messages of the chat"
    );
}

async fn check_a_summary_before_the_mutated_turn_is_kept(op: Op) {
    let c = summarized(1).await;
    prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, c.last().request_id, op).await;

    let events = expect_mutated(&resp, op);
    assert_summary_kept(&c).await;
    if op.replaces() {
        assert_eq!(
            items_of(&last_body(&c.env)),
            pairs(&[
                ("user", &summary_message("SUMMARY-TEXT")),
                ("user", if op == Op::Edit { EDITED } else { "Q-TWO" }),
            ]),
            "after a mutation of a turn that no summary covers: the summary, then the replacement"
        );
        assert!(!started(&events).data["thread_summary_applied"].is_null());
    }
}
per_op!(
    a_summary_that_ends_before_the_turn_is_kept,
    check_a_summary_before_the_mutated_turn_is_kept
);

async fn check_a_summary_ending_at_the_user_message_is_dropped(op: Op) {
    let c = summarized(2).await;
    prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, c.last().request_id, op).await;

    let events = expect_mutated(&resp, op);
    assert_summary_dropped(&c).await;
    if op.replaces() {
        assert_eq!(
            items_of(&last_body(&c.env)),
            pairs(&[
                ("user", "Q-ONE"),
                ("assistant", "A-ONE"),
                ("user", if op == Op::Edit { EDITED } else { "Q-TWO" }),
            ]),
            "until the next summary the context reads the uncompressed history"
        );
        assert!(started(&events).data["thread_summary_applied"].is_null());
    }
}
per_op!(
    a_summary_ending_at_the_user_message_of_the_turn_is_dropped,
    check_a_summary_ending_at_the_user_message_is_dropped
);

async fn check_a_summary_covering_the_turn_is_dropped(op: Op) {
    let c = summarized(3).await;
    prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, c.last().request_id, op).await;

    expect_mutated(&resp, op);
    assert_summary_dropped(&c).await;
    if op.replaces() {
        let texts: Vec<String> = items_of(&last_body(&c.env))
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert_eq!(texts.len(), 3);
        assert_eq!(&texts[..2], ["Q-ONE", "A-ONE"]);
    }
}
per_op!(
    a_summary_covering_the_turn_is_dropped,
    check_a_summary_covering_the_turn_is_dropped
);

async fn check_the_summary_goes_when_a_delete_makes_a_covered_turn_the_latest(op: Op) {
    let c = summarized(1).await;
    // A summary covering only the first turn survives the delete of the second one ...
    delete_ok(&c.env, &c.user, &c.chat, c.last().request_id).await;
    assert_summary_kept(&c).await;
    prepare(&c.env, op);

    // ... and goes when the first turn, now the latest and covered, is mutated.
    let resp = mutate(&c.env, &c.user, &c.chat, c.first().request_id, op).await;

    expect_mutated(&resp, op);
    assert_summary_dropped(&c).await;
}
per_op!(
    a_covered_turn_that_became_the_latest_after_a_delete_drops_the_summary_when_mutated,
    check_the_summary_goes_when_a_delete_makes_a_covered_turn_the_latest
);

#[tokio::test]
async fn a_mutation_does_not_touch_the_summary_of_another_chat() {
    let c = summarized(3).await;
    let other = new_chat(&c.env, &c.user).await;
    complete_turn(&c.env, &c.user, &other, "Q-OTHER", "A-OTHER").await;
    seed_summary(&c.env, &c.user, &other, "OTHER-SUMMARY", 1, 10).await;
    enqueue_text(&c.env, &[NEW_ANSWER], 10, 5);

    expect_stream(&retry(&c.env, &c.user, &c.chat, c.last().request_id).await);

    assert_summary_dropped(&c).await;
    assert_eq!(
        summary_row(&c.env, &other).await.unwrap().text,
        "OTHER-SUMMARY"
    );
    assert_eq!(
        compressed_ids(&c.env, &other).await,
        first_messages(&c.env, &other, 2).await
    );
}

// ---------------------------------------------------------------------------
// Atomicity: the summary goes together with the mutation or not at all
// ---------------------------------------------------------------------------

async fn check_a_rejected_mutation_keeps_the_covering_summary(op: Op) {
    let env = mutation::env_with_daily_cap().await;
    let c = quiet_conversation_in(env).await;
    exhaust_daily_cap(&c).await;
    let before = snapshot(&c.env, &c.chat).await;
    let calls = c.env.provider().chat_requests().len();
    prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, c.last().request_id, op).await;

    assert_quota_exhausted(&resp, "tokens");
    assert_untouched(&c.env, &c.chat, &before, calls).await;
    assert!(
        summary_row(&c.env, &c.chat).await.is_some(),
        "a rejection leaves the summary alone"
    );
}
per_replacing_op!(
    a_preflight_rejection_keeps_the_summary_and_the_flags,
    check_a_rejected_mutation_keeps_the_covering_summary
);

async fn quiet_conversation_in(env: TestEnv) -> Conversation {
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 3, 10).await;
    c
}

async fn exhaust_daily_cap(c: &Conversation) {
    mutation::exhaust_quota(&c.env, &c.user).await;
}

async fn check_the_summary_is_dropped_in_the_mutation_transaction(op: Op) {
    let c = summarized(3).await;
    exec_sql(
        &c.env,
        "CREATE TRIGGER keep_summary BEFORE DELETE ON thread_summaries \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await;
    let before = snapshot(&c.env, &c.chat).await;
    let calls = c.env.provider().chat_requests().len();
    prepare(&c.env, op);

    let resp = mutate(&c.env, &c.user, &c.chat, c.last().request_id, op).await;

    assert_problem(&resp, 500, Some("internal"));
    // Soft delete, new turn, audit event and the summary invalidation are one transaction.
    assert_untouched(&c.env, &c.chat, &before, calls).await;
    assert!(
        !turn_detail(&c.env, &c.chat, c.last().request_id)
            .await
            .deleted
    );
    assert!(summary_row(&c.env, &c.chat).await.is_some());
}
per_op!(
    the_summary_is_dropped_in_the_mutation_transaction,
    check_the_summary_is_dropped_in_the_mutation_transaction
);

#[tokio::test]
async fn a_failure_after_the_mutation_commit_does_not_bring_the_summary_back() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    seed_summary(&c.env, &c.user, &c.chat, "SUMMARY-TEXT", 1, 10).await;

    // The edit commits (and drops the covering summary), then the 20000 byte message does not
    // fit the 2900 token budget.
    let resp = mutation::edit(
        &c.env,
        &c.user,
        &c.chat,
        c.first().request_id,
        &"x".repeat(20_000),
    )
    .await;

    assert_out_of_range(&resp, "CONTEXT_BUDGET_EXCEEDED");
    assert_summary_dropped(&c).await;
    assert!(
        turn_detail(&c.env, &c.chat, c.first().request_id)
            .await
            .deleted
    );
    assert_eq!(audit_types(&c.env).await, ["turn_completed", "turn_edit"]);
}

// ---------------------------------------------------------------------------
// After a drop, the next summary starts from scratch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn after_a_drop_the_next_summary_is_built_without_a_base_frontier_over_all_live_messages() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    seed_summary(&c.env, &c.user, &c.chat, "STALE-SUMMARY", 3, 10).await;
    enqueue_text(&c.env, &["A-TWO-NEW"], 10, 5);
    expect_stream(&retry(&c.env, &c.user, &c.chat, c.last().request_id).await);
    assert!(
        summary_row(&c.env, &c.chat).await.is_none(),
        "dropped by the retry"
    );
    // Without a summary the big message reaches the threshold again: 118 + 4 * 113 + 2200.
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, "FRESH-SUMMARY");

    send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;

    let enqueued = tasks(&c.env).await;
    assert_eq!(enqueued.len(), 1, "{enqueued:?}");
    assert!(
        enqueued[0]["base_frontier_message_id"].is_null(),
        "no base frontier"
    );
    let messages = all_messages(&c.env, &c.chat).await;
    // Q-ONE, A-ONE, Q-TWO (deleted), A-TWO (deleted), Q-TWO', A-TWO', Q-THREE, A-THREE.
    assert_eq!(
        enqueued[0]["frozen_target_message_id"],
        json!(messages[5].id)
    );
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    let row = summary_row(&c.env, &c.chat).await.unwrap();
    assert_eq!(row.text.trim(), "FRESH-SUMMARY");
    let expected: Vec<Uuid> = [0, 1, 4, 5].iter().map(|i| messages[*i].id).collect();
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        expected,
        "only live messages are compressed"
    );
    let prompt = user_prompt(&summary_request(&c.env));
    assert!(
        prompt.contains("User: Q-ONE\n\nAssistant: A-ONE\n\nUser: Q-TWO\n\nAssistant: A-TWO-NEW")
    );
    assert!(!prompt.contains("STALE-SUMMARY") && !prompt.contains("<existing_summary>"));
}

#[tokio::test]
async fn a_summary_task_that_loses_against_a_mutation_leaves_nothing_behind() {
    // The task of a finished turn is still queued when a delete removes its target message.
    let c = quiet_conversation(&[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 3).await).await;
    delete_ok(&c.env, &c.user, &c.chat, c.last().request_id).await;
    enqueue_summary(&c.env, "NEVER-STORED");

    enqueue_task(&c.env, &c.user, &c.chat, None, &target).await;

    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    assert!(summary_row(&c.env, &c.chat).await.is_none());
    assert!(compressed_ids(&c.env, &c.chat).await.is_empty());
    assert!(system_usage_events(&c.env).await.is_empty());
}
