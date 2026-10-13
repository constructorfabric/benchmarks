#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Credits and tokens per model and tier, the `quota_usage` counters and the overshoot rule (DESIGN
//! 3.7 `quota_usage` commit semantics, 5.3, 5.4.4, 5.4.5, 5.7 usage accounting).
//!
//! The overshoot arithmetic uses a catalog with an expensive input (`10_000_000` micro-credits per 1M
//! tokens) and a cheap output (`1_000_000`), so that a turn whose tokens exceed the reserve also
//! costs more credits than the reserve and the cap is observable. For the 5-byte message `hello` the
//! estimated input is `113` tokens, `reserve_tokens = 113 + 4096 = 4209` and
//! `reserved_credits_micro = 113 * 10 + 4096 = 5226`.

mod common;
mod live;

use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, USAGE_QUEUE, chat_on, closing_frames,
    credits_micro, delta_frames, enqueue_chat, env_with_policy, expect_quota_row, expect_stream,
    failed_script, failing_after, message_rows, new_chat, only_turn, opening_frames, policy,
    priced_models, quota_row, quota_rows, send_text, terminal, total_reserved, web_search_frames,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const RESERVE_TOKENS: i64 = 113 + 4096;
const RESERVED_CREDITS: i64 = 113 * 10 + 4096;
const ALL_BUCKETS: [(&str, &str); 4] = [
    ("daily", "total"),
    ("monthly", "total"),
    ("daily", "tier:premium"),
    ("monthly", "tier:premium"),
];

fn expensive_input() -> Vec<Value> {
    priced_models((10_000_000, 1_000_000), (10_000_000, 1_000_000))
}

async fn overshoot_env(tolerance: Option<f64>) -> (TestEnv, TestUser, String) {
    let mut builder = TestEnvBuilder::new().policy_config(policy(expensive_input()));
    if let Some(tolerance) = tolerance {
        builder =
            builder.gear_config(json!({ "quota": { "overshoot_tolerance_factor": tolerance } }));
    }
    let env = builder.build().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

fn actual_credits(input: i64, output: i64) -> i64 {
    credits_micro(input, output, 10_000_000, 1_000_000)
}

// ===========================================================================
// Overshoot: within tolerance charges actual, beyond it caps at the reserve
// ===========================================================================

#[tokio::test]
async fn the_reserve_of_the_overshoot_fixture_is_the_documented_one() {
    let (env, user, chat) = overshoot_env(None).await;
    let stream = live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "hello", &[]).await;

    let turn = only_turn(&env, &chat).await;

    assert_eq!(turn.reserve_tokens, Some(RESERVE_TOKENS));
    assert_eq!(turn.reserved_credits_micro, Some(RESERVED_CREDITS));
    stream.disconnect();
}

#[tokio::test]
async fn an_overshoot_within_the_tolerance_is_charged_at_the_actual_credits() {
    // 4400 + 100 = 4500 tokens: 4500 / 4209 = 1.069 <= 1.10.
    let (env, user, chat) = overshoot_env(None).await;
    common::enqueue_text(&env, &["answer"], 4400, 100);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    let actual = actual_credits(4400, 100);
    assert!(
        actual > RESERVED_CREDITS,
        "the fixture costs more than the reserve"
    );
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in ALL_BUCKETS {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, actual, "{period}/{bucket}");
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
    }
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["actual_credits_micro"],
        json!(actual)
    );
}

#[tokio::test]
async fn an_overshoot_beyond_the_tolerance_is_charged_at_the_reserve() {
    // 4700 + 100 = 4800 tokens: 4800 / 4209 = 1.140 > 1.10.
    let (env, user, chat) = overshoot_env(None).await;
    common::enqueue_text(&env, &["answer"], 4700, 100);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        terminal(&events).event,
        "done",
        "the turn stays completed: {events:?}"
    );
    assert!(actual_credits(4700, 100) > RESERVED_CREDITS);
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in ALL_BUCKETS {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(
            row.spent, RESERVED_CREDITS,
            "{period}/{bucket}: capped at the reserve"
        );
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
        assert_eq!(row.calls, 1, "{period}/{bucket}");
    }
}

#[tokio::test]
async fn a_capped_turn_keeps_its_actual_token_counts_everywhere_but_the_charge() {
    let (env, user, chat) = overshoot_env(None).await;
    common::enqueue_text(&env, &["answer"], 4700, 100);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(
        done.data["usage"],
        json!({ "input_tokens": 4700, "output_tokens": 100 })
    );
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "completed");
    assert!(turn.error_code.is_none());
    let assistant = &message_rows(&env, &chat).await[1];
    assert_eq!(
        (assistant.input_tokens, assistant.output_tokens),
        (4700, 100),
        "the message keeps the provider's actual counts"
    );
    let rows = quota_rows(&env, &user).await;
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(
        (total.input_tokens, total.output_tokens),
        (4700, 100),
        "token telemetry is the actual usage"
    );
    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["usage"]["input_tokens"], json!(4700));
    assert_eq!(usage["usage"]["output_tokens"], json!(100));
    assert_eq!(
        usage["actual_credits_micro"],
        json!(RESERVED_CREDITS),
        "actual_credits_micro carries the committed credits"
    );
    assert_eq!(usage["billing_outcome"], json!("completed"));
    assert_eq!(usage["settlement_method"], json!("actual"));
    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn usage_equal_to_the_reserve_tokens_is_not_an_overshoot() {
    // 4109 + 100 = 4209 = reserve_tokens: `actual_tokens > reserve_tokens` is strict.
    let (env, user, chat) = overshoot_env(Some(1.0)).await;
    common::enqueue_text(&env, &["answer"], 4109, 100);

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        actual_credits(4109, 100)
    );
}

#[tokio::test]
async fn with_a_tolerance_of_one_any_overshoot_is_capped() {
    // 4110 + 100 = 4210 = reserve_tokens + 1.
    let (env, user, chat) = overshoot_env(Some(1.0)).await;
    common::enqueue_text(&env, &["answer"], 4110, 100);

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        RESERVED_CREDITS
    );
}

#[tokio::test]
async fn the_tolerance_factor_is_configurable_up_to_one_and_a_half() {
    // 4209 * 1.5 = 6313.5: 6313 tokens are tolerated, 6314 are not.
    let (env, user, chat) = overshoot_env(Some(1.5)).await;
    common::enqueue_text(&env, &["answer"], 6213, 100);
    expect_stream(&send_text(&env, &user, &chat, "hello").await);
    let within = env.outbox_payloads(USAGE_QUEUE).await[0].clone();

    let (env, user, chat) = overshoot_env(Some(1.5)).await;
    common::enqueue_text(&env, &["answer"], 6214, 100);
    expect_stream(&send_text(&env, &user, &chat, "hello").await);
    let beyond = env.outbox_payloads(USAGE_QUEUE).await[0].clone();

    assert_eq!(
        within["actual_credits_micro"],
        json!(actual_credits(6213, 100))
    );
    assert_eq!(beyond["actual_credits_micro"], json!(RESERVED_CREDITS));
}

#[tokio::test]
async fn a_failed_turn_with_reported_usage_follows_the_same_overshoot_rule() {
    let (env, user, chat) = overshoot_env(None).await;
    enqueue_chat(&env, failed_script("boom", Some((4400, 100))));
    expect_stream(&send_text(&env, &user, &chat, "within").await);
    let within = env.outbox_payloads(USAGE_QUEUE).await[0].clone();
    assert_eq!(within["billing_outcome"], json!("failed"));
    assert_eq!(within["settlement_method"], json!("actual"));
    assert_eq!(
        within["actual_credits_micro"],
        json!(actual_credits(4400, 100))
    );

    let (env, user, chat) = overshoot_env(None).await;
    enqueue_chat(&env, failed_script("boom", Some((4700, 100))));
    expect_stream(&send_text(&env, &user, &chat, "beyond").await);

    let beyond = env.outbox_payloads(USAGE_QUEUE).await[0].clone();
    assert_eq!(beyond["billing_outcome"], json!("failed"));
    assert_eq!(beyond["settlement_method"], json!("actual"));
    assert_eq!(beyond["actual_credits_micro"], json!(RESERVED_CREDITS));
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        RESERVED_CREDITS
    );
}

#[tokio::test]
async fn a_capped_standard_turn_is_capped_at_the_standard_reserve() {
    let env = env_with_policy(policy(priced_models(
        (10_000_000, 1_000_000),
        (20_000_000, 2_000_000),
    )))
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    common::enqueue_text(&env, &["answer"], 4700, 100);

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let standard_reserve = credits_micro(113, 4096, 20_000_000, 2_000_000);
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        standard_reserve
    );
    assert!(
        quota_row(&rows, "daily", "tier:premium").is_none_or(|row| row.spent == 0),
        "a standard turn never touches the premium subcap"
    );
}

// ===========================================================================
// Usage edge cases of a completed turn
// ===========================================================================

#[tokio::test]
async fn a_completed_turn_without_a_usage_object_is_charged_nothing() {
    let (env, user, chat) = common::env_user_chat().await;
    let mut frames = opening_frames();
    frames.extend(delta_frames(&["answer"]));
    frames.extend(closing_frames(
        "answer",
        &json!([]),
        None,
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in ALL_BUCKETS {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, 0, "{period}/{bucket}");
        assert_eq!(
            row.reserved, 0,
            "{period}/{bucket}: the reserve is released"
        );
        assert_eq!(row.calls, 1, "{period}/{bucket}");
    }
    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["settlement_method"], json!("actual"));
    assert_eq!(usage["billing_outcome"], json!("completed"));
    assert_eq!(usage["actual_credits_micro"], json!(0));
}

#[tokio::test]
async fn a_missing_usage_field_counts_as_zero() {
    let (env, user, chat) = common::env_user_chat().await;
    let mut frames = common::answer_frames(&["answer"], 7, 3);
    let last = frames.last_mut().unwrap();
    last.1["response"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("output_tokens");
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!((total.input_tokens, total.output_tokens), (7, 0));
    assert_eq!(total.spent, credits_micro(7, 0, 1_000_000, 3_000_000));
}

// ===========================================================================
// Counters per settlement method (DESIGN 3.7 commit semantics)
// ===========================================================================

#[tokio::test]
async fn calls_count_every_settlement_but_tokens_only_actual_ones() {
    let (env, user, chat) = common::env_user_chat().await;
    common::enqueue_text(&env, &["ok"], 10, 5);
    expect_stream(&send_text(&env, &user, &chat, "one").await);
    enqueue_chat(&env, failed_script("boom", None));
    expect_stream(&send_text(&env, &user, &chat, "two").await);
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "three", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let rows = quota_rows(&env, &user).await;

    for (period, bucket) in ALL_BUCKETS {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(
            row.calls, 3,
            "{period}/{bucket}: completed, failed and cancelled turns"
        );
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
    }
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(
        (total.input_tokens, total.output_tokens),
        (10, 5),
        "estimated settlements add no token telemetry"
    );
    let premium = expect_quota_row(&rows, "daily", "tier:premium");
    assert_eq!((premium.input_tokens, premium.output_tokens), (0, 0));
}

#[tokio::test]
async fn tool_call_counters_live_in_the_total_bucket_of_both_periods() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 2, Some((10, 5)))),
    );

    let events = expect_stream(
        &common::send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    let sums = |sql: &str| {
        let sql = sql.to_owned();
        let env = &env;
        async move { common::scalar_i64(env, &sql).await }
    };
    for period in ["daily", "monthly"] {
        assert_eq!(
            sums(&format!(
                "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = '{period}' AND bucket = 'total'"
            ))
            .await,
            2,
            "{period}"
        );
        assert_eq!(
            sums(&format!(
                "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = '{period}' AND bucket = 'tier:premium'"
            ))
            .await,
            0,
            "{period}"
        );
    }
}

#[tokio::test]
async fn an_estimated_settlement_still_adds_the_completed_tool_calls() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(
        &env,
        failing_after(web_search_frames(1, 1, None)[..6].to_vec(), None),
    );

    let events = expect_stream(
        &common::send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "error", "{events:?}");
    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["settlement_method"], json!("estimated"));
    assert_eq!(usage["web_search_calls"], json!(1));
    assert_eq!(
        common::scalar_i64(
            &env,
            "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = 'daily' AND bucket = 'total'"
        )
        .await,
        1,
        "web_search_calls are added on estimated settlements"
    );
}

// ===========================================================================
// Per model and tier
// ===========================================================================

#[tokio::test]
async fn each_chat_is_settled_with_the_multipliers_of_its_own_model() {
    let env = env_with_policy(policy(priced_models(
        (1_000_000, 3_000_000),
        (2_000_000, 4_000_000),
    )))
    .await;
    let user = TestUser::new();
    let premium_chat = new_chat(&env, &user).await;
    let standard_chat = chat_on(&env, &user, STANDARD_MODEL).await;
    expect_stream(&send_text(&env, &user, &premium_chat, "hello").await);
    expect_stream(&send_text(&env, &user, &standard_chat, "hello").await);

    let premium = credits_micro(10, 5, 1_000_000, 3_000_000);
    let standard = credits_micro(10, 5, 2_000_000, 4_000_000);
    assert_ne!(premium, standard);
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(total.spent, premium + standard, "{period}");
        assert_eq!(total.calls, 2, "{period}");
        let subcap = expect_quota_row(&rows, period, "tier:premium");
        assert_eq!((subcap.spent, subcap.calls), (premium, 1), "{period}");
    }
    let events = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(events[0]["effective_model"], json!(PREMIUM_MODEL));
    assert_eq!(events[0]["actual_credits_micro"], json!(premium));
    assert_eq!(events[1]["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(events[1]["actual_credits_micro"], json!(standard));
}

#[tokio::test]
async fn the_usage_credits_use_per_component_ceilings() {
    // ceil(7 * 1.5) + ceil(3 * 2.5) = 11 + 8 = 19, not ceil(18) = 18.
    let env = env_with_policy(policy(priced_models(
        (1_500_000, 2_500_000),
        (1_000_000, 3_000_000),
    )))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    common::enqueue_text(&env, &["answer"], 7, 3);

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["actual_credits_micro"],
        json!(19)
    );
}

#[tokio::test]
async fn the_audit_event_of_a_completed_turn_carries_the_actual_usage() {
    let (env, user, chat) = common::env_user_chat().await;
    common::enqueue_text(&env, &["answer"], 33, 11);
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let audit = &env.outbox_payloads(AUDIT_QUEUE).await[0];

    assert_eq!(audit["usage"]["input_tokens"], json!(33));
    assert_eq!(audit["usage"]["output_tokens"], json!(11));
}
