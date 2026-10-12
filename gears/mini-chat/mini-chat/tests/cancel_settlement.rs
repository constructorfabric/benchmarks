#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Settlement of a client disconnect (DESIGN 5.7 "client disconnect rules", 5.8 non-terminal stream
//! reconciliation): billing outcome `aborted`, settlement `estimated`, charged tokens
//! `min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied)`, credited as
//! `credits_micro(estimated_input_tokens, minimal_generation_floor_applied)` with the multipliers of
//! the effective model. Every input of the formula is read back from the persisted turn row, so the
//! expectations do not repeat the estimator.

mod common;
mod live;

use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, USAGE_QUEUE, chat_on, credits_micro, enqueue_chat,
    env_with_policy, estimated_credits, expect_quota_row, new_chat, only_turn, policy,
    priced_models, quota_row, quota_rows, scalar_i64, total_reserved, web_search_frames,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const ALL_BUCKETS: [(&str, &str); 4] = [
    ("daily", "total"),
    ("monthly", "total"),
    ("daily", "tier:premium"),
    ("monthly", "tier:premium"),
];

async fn cancelled_turn(env: &TestEnv, user: &TestUser, chat: &str, content: &str) -> Uuid {
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(env, user, chat, request_id, content, &["par"]).await;
    live::disconnect_and_wait_cancelled(env, chat, request_id, stream).await;
    request_id
}

async fn env_with_floor(floor: u32) -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .policy_config(policy(priced_models(
            (1_500_000, 2_500_000),
            (2_000_000, 4_000_000),
        )))
        .gear_config(json!({ "estimation_budgets": { "minimal_generation_floor": floor } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

#[tokio::test]
async fn a_disconnect_charges_the_estimated_input_plus_the_persisted_floor() {
    let (env, user, chat) = env_with_floor(200).await;
    cancelled_turn(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;
    let charged = estimated_credits(&turn, 1_500_000, 2_500_000);

    assert_eq!(turn.minimal_generation_floor_applied, Some(200));
    assert_eq!(
        charged,
        credits_micro(113, 200, 1_500_000, 2_500_000),
        "estimated input 113 plus the floor 200, per-component ceilings"
    );
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in ALL_BUCKETS {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, charged, "{period}/{bucket}");
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
        assert_eq!(row.calls, 1, "{period}/{bucket}");
        assert_eq!(
            (row.input_tokens, row.output_tokens),
            (0, 0),
            "{period}/{bucket}: an estimated settlement adds no token telemetry"
        );
    }
    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn the_charge_is_bounded_well_below_the_reserve() {
    let (env, user, chat) = env_with_floor(50).await;
    cancelled_turn(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;
    let reserve_tokens = turn.reserve_tokens.unwrap();
    let floor = turn.minimal_generation_floor_applied.unwrap();
    let estimated_input = reserve_tokens - turn.max_output_tokens_applied.unwrap();
    assert_eq!(
        reserve_tokens.min(estimated_input + floor),
        estimated_input + floor,
        "min(reserve_tokens, estimated_input + floor) is the second term here"
    );
    let rows = quota_rows(&env, &user).await;
    assert!(
        expect_quota_row(&rows, "daily", "total").spent < turn.reserved_credits_micro.unwrap(),
        "the full reserve is never charged"
    );
    assert!(
        expect_quota_row(&rows, "daily", "total").spent > 0,
        "no free cancel"
    );
}

#[tokio::test]
async fn the_usage_event_of_a_disconnect_is_aborted_and_estimated() {
    let (env, user, chat) = env_with_floor(200).await;
    let request_id = cancelled_turn(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;
    let usage = env.outbox_payloads(USAGE_QUEUE).await;

    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = &usage[0];
    assert_eq!(event["terminal_state"], json!("cancelled"));
    assert_eq!(event["billing_outcome"], json!("aborted"));
    assert_eq!(event["settlement_method"], json!("estimated"));
    assert_eq!(event["usage"], Value::Null);
    assert_eq!(
        event["actual_credits_micro"],
        json!(estimated_credits(&turn, 1_500_000, 2_500_000))
    );
    assert_eq!(event["effective_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["request_id"], json!(request_id.to_string()));
    let audit = env.outbox_payloads(AUDIT_QUEUE).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["event_type"], json!("turn_failed"));
    assert_eq!(audit[0]["terminal_state"], json!("cancelled"));
}

#[tokio::test]
async fn a_disconnect_is_published_once() {
    let (env, user, chat) = env_with_floor(50).await;
    cancelled_turn(&env, &user, &chat, "hello").await;

    env.drain_outbox().await;
    env.drain_outbox().await;

    let published = env.published_usage();
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0]["billing_outcome"], json!("aborted"));
}

#[tokio::test]
async fn a_standard_chat_is_charged_with_the_standard_multipliers_in_the_total_bucket_only() {
    let (env, user, _chat) = env_with_floor(200).await;
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    cancelled_turn(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;
    let charged = estimated_credits(&turn, 2_000_000, 4_000_000);

    assert_eq!(charged, credits_micro(113, 200, 2_000_000, 4_000_000));
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(
            (total.spent, total.calls, total.reserved),
            (charged, 1, 0),
            "{period}"
        );
        assert!(
            quota_row(&rows, period, "tier:premium")
                .is_none_or(|row| row.spent == 0 && row.calls == 0),
            "{period}: premium untouched"
        );
    }
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["effective_model"],
        json!(STANDARD_MODEL)
    );
}

#[tokio::test]
async fn a_downgraded_chat_is_charged_with_the_effective_models_multipliers() {
    let env = env_with_policy(common::merge(
        policy(priced_models(
            (1_500_000, 2_500_000),
            (2_000_000, 4_000_000),
        )),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    cancelled_turn(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;
    let charged = estimated_credits(&turn, 2_000_000, 4_000_000);

    let rows = quota_rows(&env, &user).await;
    assert_eq!(expect_quota_row(&rows, "daily", "total").spent, charged);
    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(usage["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(usage["actual_credits_micro"], json!(charged));
}

#[tokio::test]
async fn the_web_search_surcharge_is_part_of_the_estimated_input() {
    let (env, user, chat) = env_with_floor(50).await;
    let plain_chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, common::hang_script(&["par"]));
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
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;
    cancelled_turn(&env, &user, &plain_chat, "hello").await;

    let with_search = only_turn(&env, &chat).await;
    let plain = only_turn(&env, &plain_chat).await;

    assert_eq!(
        with_search.reserve_tokens.unwrap() - plain.reserve_tokens.unwrap(),
        500,
        "web_search_surcharge_tokens = 500"
    );
    assert_eq!(
        estimated_credits(&with_search, 1_500_000, 2_500_000)
            - estimated_credits(&plain, 1_500_000, 2_500_000),
        credits_micro(113 + 500, 50, 1_500_000, 2_500_000)
            - credits_micro(113, 50, 1_500_000, 2_500_000)
    );
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        estimated_credits(&with_search, 1_500_000, 2_500_000)
            + estimated_credits(&plain, 1_500_000, 2_500_000)
    );
}

#[tokio::test]
async fn searches_completed_before_the_disconnect_are_counted() {
    let (env, user, chat) = env_with_floor(50).await;
    let mut frames = web_search_frames(1, 1, None)[..6].to_vec();
    frames.extend(common::delta_frames(&["par"]));
    enqueue_chat(&env, ScriptedResponse::SseThenHang(frames));
    let request_id = Uuid::new_v4();
    let mut stream = live::open(
        &env,
        &user,
        &chat,
        json!({
            "content": "search", "request_id": request_id.to_string(),
            "web_search": { "enabled": true }
        }),
    )
    .await;
    live::read_until(&mut stream, "delta").await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    assert_eq!(
        scalar_i64(
            &env,
            "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = 'daily' AND bucket = 'total'"
        )
        .await,
        1,
        "estimated settlements add the completed tool calls"
    );
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["web_search_calls"],
        json!(1)
    );
}

#[tokio::test]
async fn a_disconnect_before_any_content_is_charged_like_one_after_content() {
    let (env, user, chat) = env_with_floor(100).await;
    let early_chat = new_chat(&env, &user).await;
    cancelled_turn(&env, &user, &chat, "hello").await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &early_chat, request_id, "hello", &[]).await;
    live::disconnect_and_wait_cancelled(&env, &early_chat, request_id, stream).await;

    let late = only_turn(&env, &chat).await;
    let early = only_turn(&env, &early_chat).await;

    assert_eq!(
        estimated_credits(&late, 1_500_000, 2_500_000),
        estimated_credits(&early, 1_500_000, 2_500_000),
        "the estimate does not depend on what was streamed"
    );
    let events = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(
        events[0]["actual_credits_micro"],
        events[1]["actual_credits_micro"]
    );
}
