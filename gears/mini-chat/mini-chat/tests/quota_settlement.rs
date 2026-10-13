#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Quota: preflight before any outbound call, reserve-before-execute, settlement exactly once per
//! terminal outcome, the usage and audit outbox messages and the premium -> standard downgrade
//! (DESIGN sections 3.3 "quota service", 3.8 "Two-Tier Rate Limiting", 5.3 - 5.9, ADR-0008).
//!
//! The arithmetic is derived from DESIGN: for the 5-byte message `hello` on a `catalog_entry`
//! model (`bytes_per_token_conservative` 4, `fixed_overhead_tokens` 100, `safety_margin_pct` 10,
//! `max_output_tokens` 4096, multipliers `1_000_000` / `3_000_000`) the estimated input is 113 tokens,
//! `reserve_tokens` 4209 and `reserved_credits_micro` 12401.

mod common;
mod live;

use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, USAGE_QUEUE, assert_nothing_happened,
    assert_problem, assert_quota_exhausted, catalog_entry, complete_turn, credits_micro,
    daily_period_start, dedupe_key, enqueue_chat, enqueue_text, env_two_models_with_config,
    env_with_policy, estimated_text_tokens, expect_quota_row, expect_stream, failed_script, limits,
    merge, monthly_period_start, new_chat, only_turn, policy, quota_row, quota_rows, send,
    send_text, send_with_id, terminal, tiny_limits, total_reserved, two_model_catalog,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const HELLO_BYTES: i64 = 5;
const MAX_OUTPUT: i64 = 4096;
const FLOOR: i64 = 50;

fn hello_estimate() -> i64 {
    estimated_text_tokens(HELLO_BYTES)
}

fn priced_catalog(premium: (i64, i64), standard: (i64, i64)) -> Vec<Value> {
    vec![
        merge(
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            json!({
                "input_tokens_credit_multiplier_micro": premium.0,
                "output_tokens_credit_multiplier_micro": premium.1
            }),
        ),
        merge(
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
            json!({
                "input_tokens_credit_multiplier_micro": standard.0,
                "output_tokens_credit_multiplier_micro": standard.1
            }),
        ),
    ]
}

async fn env_with_limits(
    standard: Option<Value>,
    premium: Option<Value>,
) -> (TestEnv, TestUser, String) {
    let mut config = policy(two_model_catalog());
    if let Some(standard) = standard {
        config = merge(config, json!({ "default_standard_limits": standard }));
    }
    if let Some(premium) = premium {
        config = merge(config, json!({ "default_premium_limits": premium }));
    }
    let env = env_with_policy(config).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

fn usage_event(env_payloads: &[Value], index: usize) -> &Value {
    env_payloads
        .get(index)
        .unwrap_or_else(|| panic!("no usage event #{index}: {env_payloads:?}"))
}

// ===========================================================================
// Quota is checked before any outbound provider call
// ===========================================================================

#[tokio::test]
async fn exhausted_quota_is_429_resource_exhausted_with_the_tokens_subject() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    let problem = assert_quota_exhausted(&resp, "tokens");
    assert_eq!(
        problem["context"]["violations"][0]["description"],
        json!("quota_exceeded"),
        "{problem}"
    );
}

#[tokio::test]
async fn exhausted_quota_is_a_json_problem_and_never_an_event_stream() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_problem(&resp, 429, Some("resource_exhausted"));
    assert!(common::content_type(&resp).starts_with("application/problem+json"));
}

#[tokio::test]
async fn exhausted_quota_never_reaches_the_provider_and_leaves_nothing_behind() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    send_text(&env, &user, &chat, "hello").await;

    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_daily_limit_alone_exhausts_the_tier() {
    let (env, user, chat) = env_with_limits(Some(limits(1000, 100_000_000)), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_monthly_limit_alone_exhausts_the_tier() {
    let (env, user, chat) = env_with_limits(Some(limits(100_000_000, 1000)), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_limit_that_fits_exactly_the_reserve_is_still_available() {
    let reserve = credits_micro(hello_estimate(), MAX_OUTPUT, 1_000_000, 3_000_000);
    let (env, user, chat) = env_with_limits(Some(limits(reserve, reserve)), None).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        terminal(&events).event,
        "done",
        "spent + reserved + reserve <= limit is allowed"
    );
}

#[tokio::test]
async fn a_limit_one_micro_credit_below_the_reserve_is_exhausted() {
    let reserve = credits_micro(hello_estimate(), MAX_OUTPUT, 1_000_000, 3_000_000);
    let (env, user, chat) = env_with_limits(Some(limits(reserve - 1, reserve - 1)), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
}

#[tokio::test]
async fn the_total_bucket_is_a_ceiling_over_every_tier() {
    // The premium sub-cap is generous, but the overall cap (`total`) is exhausted.
    let (env, user, chat) = env_with_limits(
        Some(tiny_limits()),
        Some(limits(1_000_000_000, 1_000_000_000)),
    )
    .await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn all_tiers_exhausted_is_429() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), Some(tiny_limits())).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
}

#[tokio::test]
async fn a_standard_chat_is_never_upgraded_to_premium_when_the_standard_cap_is_exhausted() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_standard_limits": tiny_limits() }),
    ))
    .await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn quota_is_checked_before_the_stream_opens_so_no_stream_started_is_ever_sent() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert!(
        resp.sse_events()
            .iter()
            .all(|event| event.event != "stream_started"),
        "{}",
        common::body_text(&resp)
    );
}

#[tokio::test]
async fn an_in_flight_reserve_counts_against_the_limit_of_a_parallel_request() {
    // One reserve (12401) fits into 20000, two do not.
    let (env, user, chat) = env_with_limits(Some(limits(20_000, 20_000)), None).await;
    let other_chat = new_chat(&env, &user).await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "hello", &["par"]).await;

    let resp = send_text(&env, &user, &other_chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_eq!(
        env.provider().chat_requests().len(),
        1,
        "only the running turn reached the provider"
    );
    assert_eq!(common::turn_row_count(&env).await, 1);
    stream.disconnect();
}

#[tokio::test]
async fn the_reserve_is_released_when_the_running_turn_ends_so_the_next_request_fits() {
    let (env, user, chat) = env_with_limits(Some(limits(20_000, 20_000)), None).await;
    let other_chat = new_chat(&env, &user).await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "hello", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, running, stream).await;

    let events = expect_stream(&send_text(&env, &user, &other_chat, "hello").await);

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn another_users_in_flight_reserve_does_not_count_against_me() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_standard_limits": limits(20_000, 20_000) }),
    ))
    .await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    let alice_chat = new_chat(&env, &alice).await;
    let bob_chat = new_chat(&env, &bob).await;
    let stream =
        live::start_hung_turn(&env, &alice, &alice_chat, Uuid::new_v4(), "hello", &["par"]).await;

    let events = expect_stream(&send_text(&env, &bob, &bob_chat, "hello").await);

    assert_eq!(terminal(&events).event, "done");
    stream.disconnect();
}

#[tokio::test]
async fn the_reserve_is_booked_in_every_applicable_bucket_before_the_provider_answers() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let turn = common::turn_row(&env, &chat, request_id).await.unwrap();
    let rows = quota_rows(&env, &user).await;

    let reserved = turn
        .reserved_credits_micro
        .expect("reserved_credits_micro is persisted");
    assert!(reserved > 0);
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.reserved, reserved, "{period}/{bucket}");
        assert_eq!(row.spent, 0, "{period}/{bucket}: nothing is settled yet");
    }
    stream.disconnect();
}

#[tokio::test]
async fn a_standard_turn_reserves_only_the_total_bucket() {
    let env = env_with_policy(policy(two_model_catalog())).await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let rows = quota_rows(&env, &user).await;

    let reserved = common::turn_row(&env, &chat, request_id)
        .await
        .unwrap()
        .reserved_credits_micro
        .unwrap();
    assert_eq!(expect_quota_row(&rows, "daily", "total").reserved, reserved);
    assert_eq!(
        expect_quota_row(&rows, "monthly", "total").reserved,
        reserved
    );
    assert!(
        rows.iter()
            .filter(|row| row.bucket == "tier:premium")
            .all(|row| row.reserved == 0),
        "standard turns do not reserve in the premium bucket: {rows:?}"
    );
    stream.disconnect();
}

#[tokio::test]
async fn preflight_columns_are_persisted_with_the_documented_values() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let turn = common::turn_row(&env, &chat, request_id).await.unwrap();

    let estimated_input = hello_estimate();
    assert_eq!(turn.reserve_tokens, Some(estimated_input + MAX_OUTPUT));
    assert_eq!(turn.max_output_tokens_applied, Some(MAX_OUTPUT));
    assert_eq!(
        turn.reserved_credits_micro,
        Some(credits_micro(
            estimated_input,
            MAX_OUTPUT,
            1_000_000,
            3_000_000
        ))
    );
    assert_eq!(turn.policy_version_applied, Some(1));
    assert_eq!(turn.effective_model.as_deref(), Some(PREMIUM_MODEL));
    assert_eq!(turn.minimal_generation_floor_applied, Some(FLOOR));
    stream.disconnect();
}

#[tokio::test]
async fn preflight_columns_are_immutable_through_settlement() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, common::hang_script(&["par"]));
    let mut stream = live::open(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "request_id": request_id.to_string() }),
    )
    .await;
    live::read_until(&mut stream, "delta").await;
    let before = common::turn_row(&env, &chat, request_id).await.unwrap();

    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let after = common::turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(after.reserve_tokens, before.reserve_tokens);
    assert_eq!(
        after.max_output_tokens_applied,
        before.max_output_tokens_applied
    );
    assert_eq!(after.reserved_credits_micro, before.reserved_credits_micro);
    assert_eq!(after.policy_version_applied, before.policy_version_applied);
    assert_eq!(after.effective_model, before.effective_model);
    assert_eq!(
        after.minimal_generation_floor_applied,
        before.minimal_generation_floor_applied
    );
}

#[tokio::test]
async fn max_output_tokens_applied_honours_the_streaming_cap() {
    let env =
        env_two_models_with_config(json!({ "streaming": { "max_output_tokens": 1000 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.max_output_tokens_applied,
        Some(1000),
        "min(4096, 1000)"
    );
    assert_eq!(turn.reserve_tokens, Some(hello_estimate() + 1000));
    assert_eq!(
        turn.reserved_credits_micro,
        Some(credits_micro(hello_estimate(), 1000, 1_000_000, 3_000_000))
    );
    assert_eq!(turn.minimal_generation_floor_applied, Some(FLOOR));
}

#[tokio::test]
async fn the_estimate_counts_utf8_bytes_not_characters() {
    let (env, user, chat) = common::env_user_chat().await;
    let content = "\u{e9}".repeat(10); // 20 bytes

    expect_stream(&send_text(&env, &user, &chat, &content).await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserve_tokens,
        Some(estimated_text_tokens(20) + MAX_OUTPUT)
    );
}

#[tokio::test]
async fn prior_context_tokens_of_the_last_assistant_message_enter_the_reserve() {
    let (env, user, chat) = common::env_user_chat().await;
    complete_turn(&env, &user, &chat, "hello", "answer").await; // usage 10 + 5

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turns = common::turn_rows(&env, &chat).await;
    let second = &turns[1];
    let estimated_input = hello_estimate() + 15;
    assert_eq!(second.reserve_tokens, Some(estimated_input + MAX_OUTPUT));
    assert_eq!(
        second.reserved_credits_micro,
        Some(credits_micro(
            estimated_input,
            MAX_OUTPUT,
            1_000_000,
            3_000_000
        ))
    );
}

#[tokio::test]
async fn the_web_search_surcharge_enters_the_reserve_when_the_model_supports_it() {
    let (env, user, chat) = common::env_user_chat().await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "web_search": { "enabled": true } }),
        )
        .await,
    );

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserve_tokens,
        Some(hello_estimate() + 500 + MAX_OUTPUT),
        "web_search_surcharge_tokens = 500"
    );
}

#[tokio::test]
async fn no_web_search_surcharge_is_reserved_for_a_model_without_web_search_support() {
    // The harness default standard model has `tool_support.web_search = false`.
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "web_search": { "enabled": true } }),
        )
        .await,
    );

    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.reserve_tokens, Some(hello_estimate() + MAX_OUTPUT));
}

// ===========================================================================
// Settlement of a completed turn
// ===========================================================================

#[tokio::test]
async fn a_completed_premium_turn_settles_actual_credits_in_every_bucket_and_period() {
    // Per-component ceilings: ceil(7 * 1.5) + ceil(3 * 2.5) = 11 + 8 = 19 (not ceil(18) = 18).
    let env = env_with_policy(policy(priced_catalog(
        (1_500_000, 2_500_000),
        (1_000_000, 3_000_000),
    )))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    enqueue_text(&env, &["answer"], 7, 3);

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    let expected = credits_micro(7, 3, 1_500_000, 2_500_000);
    assert_eq!(expected, 19);
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, 19, "{period}/{bucket}");
        assert_eq!(
            row.reserved, 0,
            "{period}/{bucket}: the reserve is released"
        );
        assert_eq!(row.calls, 1, "{period}/{bucket}");
    }
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (7, 3),
            "{period}"
        );
        let premium = expect_quota_row(&rows, period, "tier:premium");
        assert_eq!(
            (premium.input_tokens, premium.output_tokens),
            (0, 0),
            "token telemetry is kept in the total bucket only"
        );
    }
}

#[tokio::test]
async fn quota_rows_are_keyed_by_the_period_start() {
    let (env, user, chat) = common::env_user_chat().await;
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;

    for bucket in ["total", "tier:premium"] {
        assert_eq!(
            expect_quota_row(&rows, "daily", bucket).period_start,
            daily_period_start()
        );
        assert_eq!(
            expect_quota_row(&rows, "monthly", bucket).period_start,
            monthly_period_start()
        );
    }
}

#[tokio::test]
async fn a_completed_standard_turn_settles_with_the_standard_multipliers_in_the_total_bucket() {
    let env = env_with_policy(policy(priced_catalog(
        (1_000_000, 3_000_000),
        (2_000_000, 4_000_000),
    )))
    .await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );

    expect_stream(&send_text(&env, &user, &chat, "hello").await); // default usage 10 / 5

    let rows = quota_rows(&env, &user).await;
    let expected = credits_micro(10, 5, 2_000_000, 4_000_000);
    assert_eq!(expected, 40);
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(total.spent, 40, "{period}");
        assert_eq!(total.reserved, 0, "{period}");
        assert_eq!(total.calls, 1, "{period}");
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (10, 5),
            "{period}"
        );
        if let Some(premium) = quota_row(&rows, period, "tier:premium") {
            assert_eq!(
                (premium.spent, premium.calls),
                (0, 0),
                "{period}: premium untouched"
            );
        }
    }
}

#[tokio::test]
async fn settlements_accumulate_across_turns() {
    let (env, user, chat) = common::env_user_chat().await;
    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    let rows = quota_rows(&env, &user).await;

    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(total.spent, 2 * credits_micro(10, 5, 1_000_000, 3_000_000));
    assert_eq!(total.calls, 2);
    assert_eq!((total.input_tokens, total.output_tokens), (20, 10));
    assert_eq!(total.reserved, 0);
}

#[tokio::test]
async fn a_completed_turn_leaves_no_reserve_behind() {
    let (env, user, chat) = common::env_user_chat().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn quota_usage_is_kept_per_user() {
    let env = env_with_policy(policy(two_model_catalog())).await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    let alice_chat = new_chat(&env, &alice).await;
    complete_turn(&env, &alice, &alice_chat, "hello", "A").await;

    assert!(!quota_rows(&env, &alice).await.is_empty());
    assert!(
        quota_rows(&env, &bob).await.is_empty(),
        "bob has not used anything"
    );
}

#[tokio::test]
async fn done_after_settlement_matches_the_stored_usage() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_text(&env, &["answer"], 33, 11);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        terminal(&events).data["usage"],
        json!({ "input_tokens": 33, "output_tokens": 11 })
    );
    let rows = quota_rows(&env, &user).await;
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!((total.input_tokens, total.output_tokens), (33, 11));
}

// ===========================================================================
// Outbox: exactly one usage event and one audit event per turn
// ===========================================================================

#[tokio::test]
async fn a_completed_turn_enqueues_exactly_one_usage_event_with_the_design_fields() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_text(&env, &["answer"], 10, 5);
    expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let event = usage_event(&payloads, 0);
    let turn = only_turn(&env, &chat).await;
    assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
    assert_eq!(event["user_id"], json!(user.user_id.to_string()));
    assert_eq!(event["chat_id"], json!(chat));
    assert_eq!(event["turn_id"], json!(turn.id.to_string()));
    assert_eq!(event["request_id"], json!(request_id.to_string()));
    assert_eq!(event["effective_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["terminal_state"], json!("completed"));
    assert_eq!(event["billing_outcome"], json!("completed"));
    assert_eq!(event["settlement_method"], json!("actual"));
    assert_eq!(event["usage"]["input_tokens"], json!(10));
    assert_eq!(event["usage"]["output_tokens"], json!(5));
    assert_eq!(
        event["actual_credits_micro"],
        json!(credits_micro(10, 5, 1_000_000, 3_000_000))
    );
    assert_eq!(event["policy_version_applied"], json!(1));
    assert_eq!(event["requester_type"], json!("user"));
    assert_eq!(event["web_search_calls"], json!(0));
    assert_eq!(event["code_interpreter_calls"], json!(0));
    assert_eq!(event["file_search_calls"], json!(0));
    assert!(event["timestamp"].is_string(), "{event}");
}

#[tokio::test]
async fn the_usage_event_dedupe_key_is_tenant_turn_request_in_simple_hex() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    let turn = only_turn(&env, &chat).await;
    let expected = dedupe_key(user.tenant_id, turn.id, request_id);
    assert_eq!(payloads[0]["dedupe_key"], json!(expected));
    let parts: Vec<&str> = expected.split('/').collect();
    assert_eq!(parts.len(), 3);
    for part in parts {
        assert_eq!(part.len(), 32);
        assert!(
            part.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{part}"
        );
    }
}

#[tokio::test]
async fn the_usage_event_omits_decision_and_reserve_fields() {
    let (env, user, chat) = common::env_user_chat().await;
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    let event = usage_event(&payloads, 0);
    for absent in [
        "quota_decision",
        "downgrade_from",
        "downgrade_reason",
        "reserved_credits_micro",
        "reserve_tokens",
        "system_task_type",
        "event_type",
    ] {
        assert!(
            event.get(absent).is_none(),
            "`{absent}` is not part of the usage event: {event}"
        );
    }
}

#[tokio::test]
async fn a_completed_turn_enqueues_exactly_one_turn_completed_audit_event() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let payloads = env.outbox_payloads(AUDIT_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let event = &payloads[0];
    let turn = only_turn(&env, &chat).await;
    assert_eq!(event["event_type"], json!("turn_completed"));
    assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
    assert_eq!(event["user_id"], json!(user.user_id.to_string()));
    assert_eq!(event["chat_id"], json!(chat));
    assert_eq!(event["turn_id"], json!(turn.id.to_string()));
    assert_eq!(event["request_id"], json!(request_id.to_string()));
    assert_eq!(event["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["effective_model"], json!(PREMIUM_MODEL));
    assert_eq!(event["terminal_state"], json!("completed"));
    assert_eq!(
        event["policy_decisions"]["quota"]["decision"],
        json!("allow")
    );
}

#[tokio::test]
async fn each_turn_enqueues_its_own_usage_and_audit_events() {
    let (env, user, chat) = common::env_user_chat().await;
    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    let audit = env.outbox_payloads(AUDIT_QUEUE).await;

    assert_eq!(usage.len(), 2);
    assert_eq!(audit.len(), 2);
    assert_ne!(usage[0]["dedupe_key"], usage[1]["dedupe_key"]);
}

#[tokio::test]
async fn a_rejected_request_enqueues_no_outbox_message() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    send_text(&env, &user, &chat, "hello").await;
    send_text(&env, &user, &chat, "  ").await;

    assert!(env.outbox_payloads(USAGE_QUEUE).await.is_empty());
    assert!(env.outbox_payloads(AUDIT_QUEUE).await.is_empty());
}

#[tokio::test]
async fn draining_the_outbox_publishes_the_usage_and_audit_events_once() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    env.drain_outbox().await;
    env.drain_outbox().await;

    let published = env.published_usage();
    assert_eq!(published.len(), 1, "{published:?}");
    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        published[0]["dedupe_key"],
        json!(dedupe_key(user.tenant_id, turn.id, request_id))
    );
    let audit = env.delivered_audit();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["event_type"], json!("turn_completed"));
}

// ===========================================================================
// Settlement of a failed turn
// ===========================================================================

fn estimated_charge() -> i64 {
    // charged tokens = min(reserve, estimated_input + floor); credits use
    // credits_micro(estimated_input, floor): the full max_output is never charged.
    credits_micro(hello_estimate(), FLOOR, 1_000_000, 3_000_000)
}

#[tokio::test]
async fn a_failed_stream_without_usage_settles_the_estimated_charge_once() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, estimated_charge(), "{period}/{bucket}");
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
        assert_eq!(row.calls, 1, "{period}/{bucket}");
        assert_eq!(
            (row.input_tokens, row.output_tokens),
            (0, 0),
            "estimated settlements add no token telemetry"
        );
    }
}

#[tokio::test]
async fn a_failed_stream_without_usage_enqueues_one_failed_estimated_usage_event() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let event = &payloads[0];
    assert_eq!(event["terminal_state"], json!("failed"));
    assert_eq!(event["billing_outcome"], json!("failed"));
    assert_eq!(event["settlement_method"], json!("estimated"));
    assert_eq!(
        event["usage"],
        Value::Null,
        "the estimated split is not emitted"
    );
    assert_eq!(event["actual_credits_micro"], json!(estimated_charge()));
    assert!(
        event.get("error_code").is_none(),
        "error_code stays on the turn: {event}"
    );
}

#[tokio::test]
async fn a_failed_stream_enqueues_one_turn_failed_audit_event_with_the_error_code() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let payloads = env.outbox_payloads(AUDIT_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert_eq!(payloads[0]["event_type"], json!("turn_failed"));
    assert_eq!(payloads[0]["terminal_state"], json!("failed"));
    assert_eq!(payloads[0]["error_code"], json!("provider_error"));
}

#[tokio::test]
async fn a_failed_stream_with_provider_usage_settles_that_usage() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", Some((20, 4))));
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = quota_rows(&env, &user).await;
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(total.spent, credits_micro(20, 4, 1_000_000, 3_000_000));
    assert_eq!((total.input_tokens, total.output_tokens), (20, 4));
    assert_eq!(total.reserved, 0);
    let payloads = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["billing_outcome"], json!("failed"));
    assert_eq!(payloads[0]["settlement_method"], json!("actual"));
    assert_eq!(payloads[0]["usage"]["input_tokens"], json!(20));
    assert_eq!(payloads[0]["usage"]["output_tokens"], json!(4));
}

#[tokio::test]
async fn zero_usage_on_a_failed_stream_counts_as_unknown_and_is_estimated() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", Some((0, 0))));
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    assert_eq!(
        payloads[0]["settlement_method"],
        json!("estimated"),
        "{payloads:?}"
    );
    assert_eq!(
        payloads[0]["actual_credits_micro"],
        json!(estimated_charge())
    );
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        estimated_charge()
    );
}

#[tokio::test]
async fn a_provider_rate_limit_settles_estimated_and_releases_the_reserve() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Error {
            status: 429,
            body: json!({ "error": { "message": "slow down" } }),
            headers: vec![],
        },
    );
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(total_reserved(&env).await, 0);
    let payloads = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert_eq!(payloads[0]["billing_outcome"], json!("failed"));
    assert_eq!(payloads[0]["settlement_method"], json!("estimated"));
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn a_failed_turn_is_charged_less_than_its_reserve() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    let rows = quota_rows(&env, &user).await;

    assert!(
        expect_quota_row(&rows, "daily", "total").spent < turn.reserved_credits_micro.unwrap(),
        "the full reserve is never charged without reported usage"
    );
}

// ===========================================================================
// Settlement of a cancelled turn
// ===========================================================================

#[tokio::test]
async fn a_cancelled_turn_settles_the_estimated_charge_once() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
    ] {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, estimated_charge(), "{period}/{bucket}");
        assert_eq!(row.reserved, 0, "{period}/{bucket}");
        assert_eq!(row.calls, 1, "{period}/{bucket}");
    }
}

#[tokio::test]
async fn a_cancelled_turn_enqueues_one_aborted_estimated_usage_event() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let payloads = env.outbox_payloads(USAGE_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let event = &payloads[0];
    assert_eq!(event["terminal_state"], json!("cancelled"));
    assert_eq!(event["billing_outcome"], json!("aborted"));
    assert_eq!(event["settlement_method"], json!("estimated"));
    assert_eq!(event["usage"], Value::Null);
    assert_eq!(event["actual_credits_micro"], json!(estimated_charge()));
    assert_eq!(event["request_id"], json!(request_id.to_string()));
}

#[tokio::test]
async fn a_cancelled_turn_enqueues_a_turn_failed_audit_event() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let payloads = env.outbox_payloads(AUDIT_QUEUE).await;

    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert_eq!(
        payloads[0]["event_type"],
        json!("turn_failed"),
        "there is no turn_cancelled value"
    );
    assert_eq!(payloads[0]["terminal_state"], json!("cancelled"));
}

#[tokio::test]
async fn a_cancelled_turn_without_any_content_is_still_charged_the_floor() {
    let (env, user, chat) = common::env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &[]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let rows = quota_rows(&env, &user).await;

    assert_eq!(
        expect_quota_row(&rows, "daily", "total").spent,
        estimated_charge(),
        "no free cancel: the minimal generation floor is charged"
    );
}

#[tokio::test]
async fn settlement_happens_once_even_if_the_client_disconnects_after_done() {
    let (env, user, chat) = common::env_user_chat().await;
    enqueue_text(&env, &["answer"], 10, 5);
    let mut stream = live::open(&env, &user, &chat, json!({ "content": "hello" })).await;
    live::read_until(&mut stream, "done").await;
    stream.disconnect();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let rows = quota_rows(&env, &user).await;

    assert_eq!(expect_quota_row(&rows, "daily", "total").calls, 1);
    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), 1);
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
}

// ===========================================================================
// Downgrade (premium -> standard)
// ===========================================================================

async fn downgrade_env(premium: Value) -> (TestEnv, TestUser, String) {
    let env = env_with_policy(merge(
        policy(priced_catalog(
            (1_000_000, 3_000_000),
            (2_000_000, 4_000_000),
        )),
        json!({ "default_premium_limits": premium }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

#[tokio::test]
async fn an_exhausted_premium_bucket_downgrades_to_the_standard_model() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.event, "done", "{events:?}");
    assert_eq!(done.data["quota_decision"], json!("downgrade"));
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(done.data["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(done.data["downgrade_from"], json!(PREMIUM_MODEL));
    assert_eq!(
        done.data["downgrade_reason"],
        json!("premium_quota_exhausted")
    );
}

#[tokio::test]
async fn a_monthly_premium_limit_alone_also_downgrades() {
    let (env, user, chat) = downgrade_env(limits(100_000_000, 1000)).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["quota_decision"], json!("downgrade"));
}

#[tokio::test]
async fn a_downgraded_turn_records_the_effective_model_on_the_message_and_the_turn() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = common::message_rows(&env, &chat).await;
    assert_eq!(rows[1].model.as_deref(), Some(STANDARD_MODEL));
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.effective_model.as_deref(), Some(STANDARD_MODEL));
}

#[tokio::test]
async fn a_downgraded_turn_is_sent_to_the_standard_provider_model() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let body = common::chat_request_body(&env, 0);
    assert_eq!(body["model"], json!(format!("provider-{STANDARD_MODEL}")));
}

#[tokio::test]
async fn a_downgraded_turn_books_and_settles_with_the_standard_multipliers_in_the_total_bucket_only()
 {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserved_credits_micro,
        Some(credits_micro(
            hello_estimate(),
            MAX_OUTPUT,
            2_000_000,
            4_000_000
        )),
        "the reserve of the effective model is booked"
    );
    let rows = quota_rows(&env, &user).await;
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(total.spent, credits_micro(10, 5, 2_000_000, 4_000_000));
    assert_eq!(total.reserved, 0);
    if let Some(premium) = quota_row(&rows, "daily", "tier:premium") {
        assert_eq!((premium.spent, premium.reserved, premium.calls), (0, 0, 0));
    }
}

#[tokio::test]
async fn the_selected_model_of_the_chat_never_changes_on_a_downgrade() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let detail = common::get_chat(&env, &user, &chat).await.json();
    assert_eq!(detail["model"], json!(PREMIUM_MODEL));
    assert_eq!(
        common::chat_row(&env, &chat).await.unwrap().model,
        PREMIUM_MODEL
    );
}

#[tokio::test]
async fn a_downgrade_is_reported_in_the_usage_and_audit_events() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    let audit = env.outbox_payloads(AUDIT_QUEUE).await;

    assert_eq!(usage[0]["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(usage[0]["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(audit[0]["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(audit[0]["effective_model"], json!(STANDARD_MODEL));
    let decision = &audit[0]["policy_decisions"]["quota"];
    assert_eq!(decision["decision"], json!("downgrade"));
    assert_eq!(decision["downgrade_from"], json!(PREMIUM_MODEL));
    assert_eq!(
        decision["downgrade_reason"],
        json!("premium_quota_exhausted")
    );
}

#[tokio::test]
async fn an_available_premium_tier_is_used_without_a_downgrade() {
    let (env, user, chat) = downgrade_env(limits(1_000_000_000, 1_000_000_000)).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.data["quota_decision"], json!("allow"));
    assert_eq!(done.data["effective_model"], json!(PREMIUM_MODEL));
    let body = common::chat_request_body(&env, 0);
    assert_eq!(body["model"], json!(format!("provider-{PREMIUM_MODEL}")));
}

#[tokio::test]
async fn the_rejected_provider_model_of_a_downgrade_is_never_contacted() {
    let (env, user, chat) = downgrade_env(tiny_limits()).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    for request in env.provider().chat_requests() {
        let body = request.body_json.expect("json body");
        assert_ne!(body["model"], json!(format!("provider-{PREMIUM_MODEL}")));
    }
}

#[tokio::test]
async fn quota_problems_use_the_canonical_envelope_without_a_top_level_code() {
    let (env, user, chat) = env_with_limits(Some(tiny_limits()), None).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    let problem = assert_problem(&resp, 429, Some("resource_exhausted"));
    assert!(problem.get("code").is_none(), "{problem}");
}
