#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Tier downgrade, kill switches and the downgrade reasons of the cascade (DESIGN "Quota Before
//! Outbound" kill switches, "Downgrade Decision Flow", "Images after a downgrade", B.2.3).
//!
//! The cascade is `premium -> standard` for a premium chat and `standard` only for a standard chat
//! (a standard model never upgrades). A reason that is already set is kept: `model_disabled` (the
//! selected model is disabled) comes before a kill switch, which comes before
//! `premium_quota_exhausted`. A `hello` turn reserves `12401` micro-credits on a `catalog_entry`
//! model.

mod common;

use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, Seed, USAGE_QUEUE, assert_invalid_argument,
    assert_nothing_happened, assert_quota_exhausted, catalog_entry, chat_on, chat_request_body,
    credits_micro, done_of, env_with_kill_switches, env_with_policy, expect_quota_row,
    expect_stream, get_chat, insert_ready_image, limits, merge, message_rows, new_chat, only_turn,
    policy, priced_models, quota_row, quota_rows, seed_quota, send, send_text, set_chat_model,
    tiny_limits, total_reserved, two_model_catalog,
};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};

const HELLO_RESERVE: i64 = 12_401;

async fn env_for(entries: Vec<Value>, extra: Value) -> TestEnv {
    env_with_policy(merge(policy(entries), extra)).await
}

fn downgrade_keys_absent(done: &Value) {
    for key in ["downgrade_from", "downgrade_reason"] {
        assert!(
            done.get(key).is_none(),
            "`{key}` only on a downgrade: {done}"
        );
    }
}

// ===========================================================================
// Kill switches: force_standard_tier and disable_premium_tier
// ===========================================================================

async fn assert_kill_switch_downgrade(switch: &str, reason: &str) {
    let env = env_with_kill_switches(json!({ switch: true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["quota_decision"], json!("downgrade"), "{done:?}");
    assert_eq!(done.data["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(done.data["downgrade_from"], json!(PREMIUM_MODEL));
    assert_eq!(done.data["downgrade_reason"], json!(reason));
    assert_downgrade_persisted(&env, &user, &chat).await;
    assert_downgrade_reported(&env, reason).await;
}

async fn assert_downgrade_persisted(env: &TestEnv, user: &TestUser, chat: &str) {
    assert_eq!(
        chat_request_body(env, 0)["model"],
        json!(format!("provider-{STANDARD_MODEL}")),
        "the turn is sent to the standard provider model"
    );
    assert_eq!(
        message_rows(env, chat).await[1].model.as_deref(),
        Some(STANDARD_MODEL)
    );
    let turn = only_turn(env, chat).await;
    assert_eq!(turn.effective_model.as_deref(), Some(STANDARD_MODEL));
    assert_eq!(
        get_chat(env, user, chat).await.json()["model"],
        json!(PREMIUM_MODEL),
        "the chat's model is never rewritten"
    );
}

async fn assert_downgrade_reported(env: &TestEnv, reason: &str) {
    let audit = &env.outbox_payloads(AUDIT_QUEUE).await[0];
    assert_eq!(audit["selected_model"], json!(PREMIUM_MODEL));
    assert_eq!(audit["effective_model"], json!(STANDARD_MODEL));
    let decision = &audit["policy_decisions"]["quota"];
    assert_eq!(decision["decision"], json!("downgrade"));
    assert_eq!(decision["downgrade_from"], json!(PREMIUM_MODEL));
    assert_eq!(decision["downgrade_reason"], json!(reason));
    let usage = &env.outbox_payloads(USAGE_QUEUE).await[0];
    assert_eq!(usage["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(usage["selected_model"], json!(PREMIUM_MODEL));
}

#[tokio::test]
async fn force_standard_tier_runs_a_premium_chat_on_the_standard_model() {
    assert_kill_switch_downgrade("force_standard_tier", "force_standard_tier").await;
}

#[tokio::test]
async fn disable_premium_tier_runs_a_premium_chat_on_the_standard_model() {
    assert_kill_switch_downgrade("disable_premium_tier", "disable_premium_tier").await;
}

#[tokio::test]
async fn a_switched_off_kill_switch_changes_nothing() {
    let env = env_with_kill_switches(
        json!({ "force_standard_tier": false, "disable_premium_tier": false }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["quota_decision"], json!("allow"));
    assert_eq!(done.data["effective_model"], json!(PREMIUM_MODEL));
    downgrade_keys_absent(&done.data);
}

#[tokio::test]
async fn a_kill_switch_reason_wins_over_an_exhausted_premium_quota() {
    let env = env_for(
        two_model_catalog(),
        json!({
            "kill_switches": { "force_standard_tier": true },
            "default_premium_limits": tiny_limits(),
        }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        done_of(&events).data["downgrade_reason"],
        json!("force_standard_tier"),
        "the first reason that is set is kept"
    );
}

#[tokio::test]
async fn a_standard_chat_is_not_affected_by_the_premium_kill_switches() {
    for switch in ["force_standard_tier", "disable_premium_tier"] {
        let env = env_with_kill_switches(json!({ switch: true })).await;
        let user = TestUser::new();
        let chat = chat_on(&env, &user, STANDARD_MODEL).await;

        let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

        let done = done_of(&events);
        assert_eq!(done.data["quota_decision"], json!("allow"), "{switch}");
        assert_eq!(
            done.data["effective_model"],
            json!(STANDARD_MODEL),
            "{switch}"
        );
        downgrade_keys_absent(&done.data);
    }
}

#[tokio::test]
async fn a_kill_switch_downgrade_books_and_settles_with_the_standard_model_in_the_total_bucket() {
    let env = env_for(
        priced_models((1_000_000, 3_000_000), (2_000_000, 4_000_000)),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        done_of(&events).data["effective_model"],
        json!(STANDARD_MODEL)
    );
    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserved_credits_micro,
        Some(credits_micro(113, 4096, 2_000_000, 4_000_000)),
        "the reserve of the effective model"
    );
    let rows = quota_rows(&env, &user).await;
    for period in ["daily", "monthly"] {
        let total = expect_quota_row(&rows, period, "total");
        assert_eq!(
            total.spent,
            credits_micro(10, 5, 2_000_000, 4_000_000),
            "{period}"
        );
        assert_eq!(total.reserved, 0, "{period}");
        if let Some(premium) = quota_row(&rows, period, "tier:premium") {
            assert_eq!((premium.spent, premium.reserved, premium.calls), (0, 0, 0));
        }
    }
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["actual_credits_micro"],
        json!(credits_micro(10, 5, 2_000_000, 4_000_000))
    );
}

#[tokio::test]
async fn a_kill_switch_with_an_exhausted_standard_tier_is_429_tokens() {
    let env = env_for(
        two_model_catalog(),
        json!({
            "kill_switches": { "force_standard_tier": true },
            "default_standard_limits": tiny_limits(),
        }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_premium_kill_switch_without_an_enabled_standard_model_is_429_tokens() {
    for switch in ["force_standard_tier", "disable_premium_tier"] {
        let env = env_for(
            vec![catalog_entry(PREMIUM_MODEL, "Premium", true, true)],
            json!({ "kill_switches": { switch: true } }),
        )
        .await;
        let user = TestUser::new();
        let chat = new_chat(&env, &user).await;

        let resp = send_text(&env, &user, &chat, "hello").await;

        assert_quota_exhausted(&resp, "tokens");
        assert_nothing_happened(&env).await;
    }
}

// ===========================================================================
// A disabled chat model (enabled = false after the chat was created)
// ===========================================================================

fn model_id(tier: &str, name: &str) -> String {
    format!("gpt-{tier}-{name}")
}

#[tokio::test]
async fn a_disabled_premium_model_falls_back_to_the_default_enabled_premium_model() {
    let env = env_for(
        vec![
            catalog_entry(&model_id("premium", "a"), "Premium", true, false),
            catalog_entry(&model_id("premium", "b"), "Premium", true, true),
            catalog_entry(&model_id("premium", "c"), "Premium", false, false),
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ],
        json!({}),
    )
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, &model_id("premium", "a")).await;
    set_chat_model(&env, &chat, &model_id("premium", "c")).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["quota_decision"], json!("downgrade"));
    assert_eq!(done.data["selected_model"], json!(model_id("premium", "c")));
    assert_eq!(done.data["downgrade_from"], json!(model_id("premium", "c")));
    assert_eq!(done.data["downgrade_reason"], json!("model_disabled"));
    assert_eq!(
        done.data["effective_model"],
        json!(model_id("premium", "b")),
        "the preferred (is_default) model of the tier"
    );
    assert_eq!(
        chat_request_body(&env, 0)["model"],
        json!(format!("provider-{}", model_id("premium", "b")))
    );
    let rows = quota_rows(&env, &user).await;
    assert_eq!(
        expect_quota_row(&rows, "daily", "tier:premium").calls,
        1,
        "the effective model is premium: both buckets are settled"
    );
    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["model"],
        json!(model_id("premium", "c")),
        "the chat keeps its model"
    );
}

#[tokio::test]
async fn without_a_preferred_model_the_first_enabled_model_of_the_tier_is_used() {
    let env = env_for(
        vec![
            catalog_entry(&model_id("premium", "a"), "Premium", true, false),
            catalog_entry(&model_id("premium", "b"), "Premium", true, false),
            catalog_entry(&model_id("premium", "c"), "Premium", false, false),
            catalog_entry(STANDARD_MODEL, "Standard", true, true),
        ],
        json!({}),
    )
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, &model_id("premium", "a")).await;
    set_chat_model(&env, &chat, &model_id("premium", "c")).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        done_of(&events).data["effective_model"],
        json!(model_id("premium", "a")),
        "catalog order"
    );
}

#[tokio::test]
async fn a_disabled_premium_model_without_an_enabled_sibling_cascades_to_standard() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", false, false),
            catalog_entry(STANDARD_MODEL, "Standard", true, true),
        ],
        json!({}),
    )
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    set_chat_model(&env, &chat, PREMIUM_MODEL).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(done.data["downgrade_reason"], json!("model_disabled"));
    assert_eq!(done.data["downgrade_from"], json!(PREMIUM_MODEL));
}

#[tokio::test]
async fn model_disabled_is_kept_when_the_premium_quota_is_exhausted_too() {
    let env = env_for(
        vec![
            catalog_entry(&model_id("premium", "a"), "Premium", true, true),
            catalog_entry(&model_id("premium", "b"), "Premium", false, false),
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ],
        json!({ "default_premium_limits": tiny_limits() }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    set_chat_model(&env, &chat, &model_id("premium", "b")).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(
        done.data["downgrade_reason"],
        json!("model_disabled"),
        "the reason set first is kept"
    );
}

#[tokio::test]
async fn a_disabled_standard_model_falls_back_within_the_standard_tier_and_never_upgrades() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            catalog_entry(&model_id("standard", "a"), "Standard", false, false),
            catalog_entry(&model_id("standard", "b"), "Standard", true, false),
        ],
        json!({}),
    )
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, &model_id("standard", "b")).await;
    set_chat_model(&env, &chat, &model_id("standard", "a")).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(
        done.data["effective_model"],
        json!(model_id("standard", "b"))
    );
    assert_eq!(done.data["downgrade_reason"], json!("model_disabled"));
    assert_eq!(
        done.data["downgrade_from"],
        json!(model_id("standard", "a"))
    );
    let rows = quota_rows(&env, &user).await;
    assert!(
        quota_row(&rows, "daily", "tier:premium").is_none_or(|row| row.calls == 0),
        "premium is never used by a standard chat"
    );
}

#[tokio::test]
async fn a_disabled_model_reports_its_reason_in_the_audit_event() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", false, false),
            catalog_entry(STANDARD_MODEL, "Standard", true, true),
        ],
        json!({}),
    )
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    set_chat_model(&env, &chat, PREMIUM_MODEL).await;
    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let audit = &env.outbox_payloads(AUDIT_QUEUE).await[0];

    let decision = &audit["policy_decisions"]["quota"];
    assert_eq!(decision["decision"], json!("downgrade"));
    assert_eq!(decision["downgrade_reason"], json!("model_disabled"));
    assert_eq!(decision["downgrade_from"], json!(PREMIUM_MODEL));
}

// ===========================================================================
// A standard model never upgrades
// ===========================================================================

#[tokio::test]
async fn a_standard_chat_stays_on_the_standard_model_while_premium_has_plenty_of_quota() {
    let env = env_with_policy(policy(two_model_catalog())).await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["quota_decision"], json!("allow"));
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    downgrade_keys_absent(&done.data);
    assert_eq!(
        chat_request_body(&env, 0)["model"],
        json!(format!("provider-{STANDARD_MODEL}"))
    );
}

// ===========================================================================
// premium_quota_exhausted at the exact boundary of the reserve
// ===========================================================================

#[tokio::test]
async fn premium_stays_available_while_the_reserve_still_fits_the_subcap() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_premium_limits": limits(1_000_000, 1_000_000_000) }),
    )
    .await;
    let user = TestUser::new();
    seed_quota(
        &env,
        &user,
        "daily",
        "tier:premium",
        Seed::spent(1_000_000 - HELLO_RESERVE),
    )
    .await;
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(done_of(&events).data["quota_decision"], json!("allow"));
}

#[tokio::test]
async fn one_micro_credit_less_room_downgrades_with_premium_quota_exhausted() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_premium_limits": limits(1_000_000, 1_000_000_000) }),
    )
    .await;
    let user = TestUser::new();
    seed_quota(
        &env,
        &user,
        "daily",
        "tier:premium",
        Seed::spent(1_000_000 - HELLO_RESERVE + 1),
    )
    .await;
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["quota_decision"], json!("downgrade"));
    assert_eq!(
        done.data["downgrade_reason"],
        json!("premium_quota_exhausted")
    );
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
}

#[tokio::test]
async fn an_exhausted_monthly_premium_row_downgrades_too() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_premium_limits": limits(1_000_000_000, 5_000) }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        done_of(&events).data["downgrade_reason"],
        json!("premium_quota_exhausted")
    );
}

#[tokio::test]
async fn the_downgrade_is_decided_per_turn_and_premium_returns_once_it_has_room() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_premium_limits": limits(1_000_000, 1_000_000_000) }),
    )
    .await;
    let user = TestUser::new();
    seed_quota(&env, &user, "daily", "tier:premium", Seed::spent(1_000_000)).await;
    let chat = new_chat(&env, &user).await;
    let first = expect_stream(&send_text(&env, &user, &chat, "one").await);
    assert_eq!(done_of(&first).data["quota_decision"], json!("downgrade"));

    seed_quota(&env, &user, "daily", "tier:premium", Seed::spent(0)).await;
    let second = expect_stream(&send_text(&env, &user, &chat, "two").await);

    let done = done_of(&second);
    assert_eq!(done.data["quota_decision"], json!("allow"), "{done:?}");
    assert_eq!(done.data["effective_model"], json!(PREMIUM_MODEL));
}

#[tokio::test]
async fn a_downgraded_and_a_premium_turn_accumulate_with_their_own_multipliers() {
    // The first turn reserves 12401 and fits the premium subcap of 12420; once it has spent 25 the
    // second reserve (25 + 12401 = 12426) no longer fits, so the second turn runs on standard.
    let env = env_for(
        priced_models((1_000_000, 3_000_000), (2_000_000, 4_000_000)),
        json!({ "default_premium_limits": limits(12_420, 1_000_000_000) }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let first = expect_stream(&send_text(&env, &user, &chat, "hello").await);
    let second = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(done_of(&first).data["quota_decision"], json!("allow"));
    assert_eq!(done_of(&second).data["quota_decision"], json!("downgrade"));
    let rows = quota_rows(&env, &user).await;
    let premium_credits = credits_micro(10, 5, 1_000_000, 3_000_000);
    let standard_credits = credits_micro(10, 5, 2_000_000, 4_000_000);
    let total = expect_quota_row(&rows, "daily", "total");
    assert_eq!(total.spent, premium_credits + standard_credits);
    assert_eq!(total.calls, 2);
    let premium = expect_quota_row(&rows, "daily", "tier:premium");
    assert_eq!(
        (premium.spent, premium.calls),
        (premium_credits, 1),
        "only the premium turn is counted in the subcap"
    );
    assert_eq!(total_reserved(&env).await, 0);
}

// ===========================================================================
// All tiers exhausted
// ===========================================================================

#[tokio::test]
async fn when_the_overall_cap_has_no_room_for_the_reserve_every_tier_is_exhausted() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_standard_limits": limits(1_000_000, 1_000_000_000) }),
    )
    .await;
    let user = TestUser::new();
    seed_quota(
        &env,
        &user,
        "daily",
        "total",
        Seed::spent(1_000_000 - HELLO_RESERVE + 1),
    )
    .await;
    let premium_chat = new_chat(&env, &user).await;
    let standard_chat = chat_on(&env, &user, STANDARD_MODEL).await;

    for chat in [&premium_chat, &standard_chat] {
        let resp = send_text(&env, &user, chat, "hello").await;
        let problem = assert_quota_exhausted(&resp, "tokens");
        assert_eq!(problem["status"], json!(429));
    }
    assert!(env.provider().chat_requests().is_empty());
    assert_eq!(total_reserved(&env).await, 0);
}

#[tokio::test]
async fn a_monthly_overall_cap_exhausts_every_tier_too() {
    let env = env_for(
        two_model_catalog(),
        json!({ "default_standard_limits": limits(1_000_000_000, 5_000) }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert_nothing_happened(&env).await;
}

// ===========================================================================
// VISION_INPUT is checked on the effective model
// ===========================================================================

fn without_vision(mut entry: Value) -> Value {
    entry["multimodal_capabilities"] = json!([]);
    entry
}

fn image_body(content: &str, image: uuid::Uuid) -> Value {
    json!({ "content": content, "attachment_ids": [image.to_string()] })
}

#[tokio::test]
async fn a_kill_switch_downgrade_to_a_model_without_vision_rejects_an_image() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            without_vision(catalog_entry(STANDARD_MODEL, "Standard", true, false)),
        ],
        json!({ "kill_switches": { "force_standard_tier": true } }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_ready_image(&env, &user, &chat, "file-vision0000000001").await;

    let resp = send(&env, &user, &chat, image_body("what is this", image)).await;

    assert_invalid_argument(&resp, "content_type", "VISION_NOT_SUPPORTED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_quota_downgrade_to_a_model_without_vision_rejects_an_image_and_reserves_nothing() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            without_vision(catalog_entry(STANDARD_MODEL, "Standard", true, false)),
        ],
        json!({ "default_premium_limits": tiny_limits() }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_ready_image(&env, &user, &chat, "file-vision0000000002").await;

    let resp = send(&env, &user, &chat, image_body("what is this", image)).await;

    assert_invalid_argument(&resp, "content_type", "VISION_NOT_SUPPORTED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_downgrade_to_a_model_with_vision_accepts_an_image_even_if_the_selected_model_has_none() {
    let env = env_for(
        vec![
            without_vision(catalog_entry(PREMIUM_MODEL, "Premium", true, true)),
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ],
        json!({ "kill_switches": { "force_standard_tier": true } }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_ready_image(&env, &user, &chat, "file-vision0000000003").await;

    let events = expect_stream(&send(&env, &user, &chat, image_body("what is this", image)).await);

    assert_eq!(
        done_of(&events).data["effective_model"],
        json!(STANDARD_MODEL)
    );
    let body = chat_request_body(&env, 0);
    assert_eq!(body["model"], json!(format!("provider-{STANDARD_MODEL}")));
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"][1],
        json!({ "type": "input_image", "file_id": "file-vision0000000003" })
    );
}

#[tokio::test]
async fn a_downgrade_to_a_model_without_vision_still_serves_a_text_only_turn() {
    let env = env_for(
        vec![
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            without_vision(catalog_entry(STANDARD_MODEL, "Standard", true, false)),
        ],
        json!({ "kill_switches": { "force_standard_tier": true } }),
    )
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        done_of(&events).data["effective_model"],
        json!(STANDARD_MODEL)
    );
}

#[tokio::test]
async fn without_a_downgrade_the_premium_model_accepts_an_image() {
    let env = env_for(two_model_catalog(), json!({})).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_ready_image(&env, &user, &chat, "file-vision0000000004").await;

    let events = expect_stream(&send(&env, &user, &chat, image_body("what is this", image)).await);

    assert_eq!(done_of(&events).data["quota_decision"], json!("allow"));
}
