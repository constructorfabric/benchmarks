//! Quota before outbound, reserve before execute and the tier downgrade (the preflight half of
//! slice S4): `POST /v1/chats/{id}/messages:stream` with quota limits, kill switches and a
//! database that can be read at the instant the provider is called.
//!
//! Source of truth: DESIGN 2.2 "Quota Before Outbound" and kill switches, 3.6 "Send Message with
//! Streaming Response" (order of the preflight steps, single reserve transaction), 3.7
//! `chat_turns` preflight columns / `quota_usage`, 4 "Two-Tier Rate Limiting & Throttling",
//! "Downgrade Decision Flow", "Effective Model Computed Once", 5.4.1 - 5.4.3, 5.5.4 / 5.5.6, the
//! REST error table (429 `resource_exhausted`, `violations[{subject, description}]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use chat_support::{assert_field_violation, assert_precondition};
use quota_support::{
    DISABLED, LimitsSpec, MessageSeed, PREMIUM, QuotaHarness, QuotaOptions, STANDARD, USER_A,
    UsageSeed, answer_with_usage, assert_exhausted, assert_problem, ctx_a, find_model,
    quota_models, ref_credits, ref_reserve, standard_two,
};
use serde_json::{Value, json};
use stream_support::{
    Opts, ProviderReply, TENANT_B, USER_B, assert_400_reason, provider_model, second_user,
    tenant_b_user,
};
use uuid::Uuid;

const MSG: &str = "hello";

/// `(estimated_input, max_output, reserve_tokens, reserved_credits)` of `MSG` on `model`.
fn reserve_of(model: &str) -> (u64, u32, u64, i64) {
    ref_reserve(&find_model(&quota_models(), model), 32_768, MSG, 0, 0)
}

fn premium_credits() -> i64 {
    reserve_of(PREMIUM).3
}

fn standard_credits() -> i64 {
    reserve_of(STANDARD).3
}

/// Limits that leave plenty of room in every bucket except where a test seeds usage.
fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000)
}

// ═════════════════════════════════════════════════════════════════════════════
// the reference numbers the other tests build on
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn the_reference_reserve_of_a_five_byte_message() {
    // ceil((ceil(5 / 4) + 100) * 110 / 100) = 113 input tokens; output cap 4096
    assert_eq!(reserve_of(PREMIUM), (113, 4096, 4209, 61_779));
    assert_eq!(reserve_of(STANDARD), (113, 4096, 4209, 12_401));
    assert_eq!(ref_credits(113, 4096, 3_000_000, 15_000_000), 61_779);
}

// ═════════════════════════════════════════════════════════════════════════════
// quota before outbound: 429 resource_exhausted, JSON, nothing written, provider untouched
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn all_tiers_exhausted_is_a_429_tokens_rejection_before_any_provider_call() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat().await;
    h.seed_usage(UsageSeed::total("daily").spent(1_000_000))
        .await;
    let before = h.row_counts().await;
    let chat_before = h.chat.chat_row(chat).await.unwrap().updated_at;

    let resp = h.reject(chat, json!({"content": MSG})).await;

    let problem = assert_exhausted(&resp, "tokens");
    assert_eq!(problem["status"], json!(429));
    h.oagw.assert_no_provider_call();
    assert_eq!(
        h.row_counts().await,
        before,
        "no message, turn or quota row"
    );
    assert_eq!(
        h.chat.chat_row(chat).await.unwrap().updated_at,
        chat_before,
        "chats.updated_at is not bumped by a rejected send"
    );
    h.policy.assert_published_stays(0).await;
    h.audit.assert_events_stay(0).await;
}

#[tokio::test]
async fn an_exhausted_shared_cap_in_either_period_rejects_both_models() {
    for period in ["daily", "monthly"] {
        let h = QuotaHarness::with_limits(LimitsSpec::new(
            1_000_000,
            10_000_000,
            500_000_000,
            5_000_000_000,
        ))
        .await;
        let limit = if period == "daily" {
            1_000_000
        } else {
            10_000_000
        };
        h.seed_usage(UsageSeed::total(period).spent(limit)).await;
        for model in [PREMIUM, STANDARD] {
            let chat = h.seed_chat_on(model).await;
            let resp = h.reject(chat, json!({"content": MSG})).await;
            assert_exhausted(&resp, "tokens");
        }
        h.oagw.assert_no_provider_call();
    }
}

#[tokio::test]
async fn the_reserve_must_fit_the_remaining_limit_and_an_exact_fit_is_admitted() {
    let reserve = standard_credits();
    let too_small = u64::try_from(reserve - 1).unwrap();
    let exact = u64::try_from(reserve).unwrap();

    let h = QuotaHarness::with_limits(LimitsSpec::new(
        too_small,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    let resp = h.reject(chat, json!({"content": MSG})).await;
    assert_exhausted(&resp, "tokens");

    let h = QuotaHarness::with_limits(LimitsSpec::new(
        exact,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    let sent = h.say(chat, MSG).await;
    sent.assert_well_formed();
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn reserves_of_other_in_flight_turns_count_against_the_limit() {
    let reserve = standard_credits();
    // 20_000 daily: 12_000 already reserved by others leaves 8_000 < 12_401
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        20_000,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    h.seed_usage(UsageSeed::total("daily").reserved(12_000))
        .await;
    let chat = h.seed_chat_on(STANDARD).await;
    assert_exhausted(&h.reject(chat, json!({"content": MSG})).await, "tokens");

    // 20_000 - 12_401 = 7_599 reserved by others still fits exactly
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        20_000,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    h.seed_usage(UsageSeed::total("daily").reserved(20_000 - reserve))
        .await;
    let chat = h.seed_chat_on(STANDARD).await;
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn an_in_flight_turn_of_the_same_user_blocks_a_second_chat_when_the_reserve_does_not_fit() {
    let reserve = standard_credits();
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        u64::try_from(reserve).unwrap() + 100,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    let first = h.seed_chat_on(STANDARD).await;
    let second = h.seed_chat_on(STANDARD).await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(first, &ctx_a(), &json!({"content": MSG}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    assert_eq!(h.amounts("total", "daily").await, (0, reserve));

    let resp = h.reject(second, json!({"content": MSG})).await;
    assert_exhausted(&resp, "tokens");

    h.oagw.release();
    live.drain().await;
    assert_eq!(h.amounts("total", "daily").await.1, 0);
}

#[tokio::test]
async fn other_users_and_tenants_have_their_own_quota() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    // the second user of tenant A and the user of tenant B are exhausted, USER_A is not
    h.seed_usage(
        UsageSeed::total("daily")
            .spent(1_000_000)
            .user(chat_support::USER_A2, quota_support::TENANT_A),
    )
    .await;
    h.seed_usage(
        UsageSeed::total("daily")
            .spent(1_000_000)
            .user(USER_B, TENANT_B),
    )
    .await;

    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    let other = h
        .seed_chat_of(chat_support::USER_A2, quota_support::TENANT_A, PREMIUM)
        .await;
    let resp = h
        .reject_as(other, &second_user(), json!({"content": MSG}))
        .await;
    assert_exhausted(&resp, "tokens");
    let foreign = h.seed_chat_of(USER_B, TENANT_B, PREMIUM).await;
    let resp = h
        .reject_as(foreign, &tenant_b_user(), json!({"content": MSG}))
        .await;
    assert_exhausted(&resp, "tokens");
}

#[tokio::test]
async fn the_quota_rows_of_a_user_are_keyed_by_tenant_and_user() {
    let h = QuotaHarness::new().await;
    let chat_a = h.seed_chat().await;
    let chat_b = h.seed_chat_of(USER_B, TENANT_B, PREMIUM).await;
    h.say(chat_a, MSG).await;
    let sent_b = h
        .open(chat_b, &tenant_b_user(), &json!({"content": MSG}))
        .await
        .collect()
        .await;
    assert_eq!(sent_b.names_no_ping().last().unwrap(), "done");

    let rows = h.usage_rows().await;
    for row in &rows {
        let owner = (row.tenant_id, row.user_id);
        assert!(
            owner == (quota_support::TENANT_A, USER_A) || owner == (TENANT_B, USER_B),
            "{row:?}"
        );
    }
    assert!(rows.iter().any(|r| r.user_id == USER_A));
    assert!(rows.iter().any(|r| r.user_id == USER_B));
    let a = h.bucket_of(USER_A, "total", "daily").await.unwrap();
    let b = h.bucket_of(USER_B, "total", "daily").await.unwrap();
    assert_eq!((a.calls, b.calls), (1, 1));
}

// ═════════════════════════════════════════════════════════════════════════════
// order of the preflight checks (DESIGN 3.6 sequence)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn validation_and_the_web_search_kill_switch_come_before_the_quota_check() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat().await;
    h.seed_usage(UsageSeed::total("daily").spent(1_000_000))
        .await;

    // empty content: 400 even though the quota is gone
    let resp = h.reject(chat, json!({"content": "   "})).await;
    let problem = assert_400_reason(&resp, "invalid_argument", "EMPTY_CONTENT");
    assert_field_violation(&problem, "content", "EMPTY_CONTENT");

    // web_search with the kill switch on: 400 FEATURE_DISABLED, not 429
    h.policy
        .set_kill_switches(json!({"disable_web_search": true}));
    let resp = h
        .reject(
            chat,
            json!({"content": MSG, "web_search": {"enabled": true}}),
        )
        .await;
    let problem = assert_problem(&resp, 400, "failed_precondition");
    assert_precondition(&problem, "web_search", "FEATURE_DISABLED");
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn the_quota_check_comes_before_the_input_length_check() {
    let long = "x".repeat(450_000); // ~123_860 estimated tokens > max_input_tokens 100_000
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat().await;

    // within quota the message is too long
    let roomy_h = QuotaHarness::with_limits(roomy()).await;
    let roomy_chat = roomy_h.seed_chat().await;
    let resp = roomy_h.reject(roomy_chat, json!({"content": long})).await;
    assert_400_reason(&resp, "out_of_range", "INPUT_TOO_LONG");

    // with every tier exhausted the same message is a quota rejection
    h.seed_usage(UsageSeed::total("daily").spent(1_000_000))
        .await;
    let resp = h.reject(chat, json!({"content": long})).await;
    assert_exhausted(&resp, "tokens");
}

#[tokio::test]
async fn a_replay_of_a_completed_turn_wins_over_an_exhausted_quota() {
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        10_000_000,
        100_000_000,
        5_000_000,
        50_000_000,
    ))
    .await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    let first = h.say_with(chat, MSG, rid).await;
    assert_eq!(first.names_no_ping().last().unwrap(), "done");

    // the quota is gone now, the completed key is still replayed (side-effect free)
    h.seed_usage(UsageSeed::total("monthly").spent(100_000_000))
        .await;
    let fingerprint = h.usage_fingerprint().await;
    let calls = h.oagw.request_count();
    let replay = h.say_with(chat, MSG, rid).await;
    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert_eq!(replay.names_no_ping().last().unwrap(), "done");
    assert_eq!(h.oagw.request_count(), calls, "no provider call");
    assert_eq!(
        h.usage_fingerprint().await,
        fingerprint,
        "no reserve, no debit"
    );
}

#[tokio::test]
async fn the_parallel_turn_guard_wins_over_an_exhausted_quota() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;

    h.seed_usage(UsageSeed::total("daily").spent(1_000_000_000))
        .await;
    let resp = h.reject(chat, json!({"content": MSG})).await;
    stream_support::assert_conflict(&resp, "turn_already_running");

    h.oagw.release();
    live.drain().await;
}

// ═════════════════════════════════════════════════════════════════════════════
// reserve before execute: chat_turns preflight columns and quota_usage rows
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_reserve_is_committed_before_the_provider_is_called() {
    let h = QuotaHarness::with(QuotaOptions {
        limits: roomy(),
        policy_version: 7,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let (_, max_out, reserve_tokens, reserve_credits) = reserve_of(PREMIUM);

    let sent = h.say(chat, MSG).await;
    let rid = sent.request_id();
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(h.oagw.request_count(), 1);

    let seen = h.observed.snapshot(0);
    let turn = seen
        .turns
        .iter()
        .find(|t| t.request_id == rid)
        .expect("the turn was committed when the provider was called");
    assert_eq!(turn.state, "running");
    assert_eq!(
        turn.reserve_tokens,
        Some(i64::try_from(reserve_tokens).unwrap())
    );
    assert_eq!(
        turn.max_output_tokens_applied,
        Some(i32::try_from(max_out).unwrap())
    );
    assert_eq!(turn.reserved_credits_micro, Some(reserve_credits));
    assert_eq!(turn.policy_version_applied, Some(7));
    assert_eq!(turn.effective_model.as_deref(), Some(PREMIUM));
    assert_eq!(turn.minimal_generation_floor_applied, Some(50));

    // premium turn: total and tier:premium, daily and monthly, all reserved; nothing spent yet
    assert_eq!(seen.usage.len(), 4, "{:?}", seen.usage);
    for (bucket, period) in [
        ("total", "daily"),
        ("total", "monthly"),
        ("tier:premium", "daily"),
        ("tier:premium", "monthly"),
    ] {
        let row = seen
            .usage
            .iter()
            .find(|r| r.bucket == bucket && r.period_type == period)
            .unwrap_or_else(|| panic!("no {bucket}/{period} row at provider call time"));
        assert_eq!(
            row.reserved_credits_micro, reserve_credits,
            "{bucket}/{period}"
        );
        assert_eq!(row.spent_credits_micro, 0, "{bucket}/{period}");
        assert_eq!(row.calls, 0, "{bucket}/{period}");
        assert_eq!(
            (row.tenant_id, row.user_id),
            (quota_support::TENANT_A, USER_A)
        );
    }
}

#[tokio::test]
async fn the_preflight_columns_are_immutable_after_the_turn_ends() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let rid = sent.request_id();
    let seen = h.observed.snapshot(0);
    let before = seen
        .turns
        .iter()
        .find(|t| t.request_id == rid)
        .unwrap()
        .clone();
    let after = h.turn_of(rid).await.unwrap();
    assert_eq!(after.state, "completed");
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
async fn a_standard_turn_reserves_only_the_total_bucket() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.say(chat, MSG).await;
    let seen = h.observed.snapshot(0);
    assert_eq!(seen.usage.len(), 2, "{:?}", seen.usage);
    assert!(seen.usage.iter().all(|r| r.bucket == "total"));
    assert!(
        seen.usage
            .iter()
            .all(|r| r.reserved_credits_micro == standard_credits())
    );
    assert!(
        h.bucket("tier:premium", "daily").await.is_none(),
        "a standard-tier turn never creates a premium row"
    );
}

#[tokio::test]
async fn period_starts_are_the_utc_day_and_the_first_of_the_month() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let day_before = time::OffsetDateTime::now_utc().date();
    h.say(chat, MSG).await;
    let day_after = time::OffsetDateTime::now_utc().date();

    let rows = h.usage_rows().await;
    assert_eq!(rows.len(), 4);
    let text = |date: time::Date| {
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    };
    let daily = [text(day_before), text(day_after)];
    let monthly = [
        format!("{}-01", &text(day_before)[..7]),
        format!("{}-01", &text(day_after)[..7]),
    ];
    for row in rows {
        let start = row.period_start.to_string();
        match row.period_type.as_str() {
            "daily" => assert!(daily.contains(&start), "daily start {start}"),
            "monthly" => assert!(monthly.contains(&start), "monthly start {start}"),
            other => panic!("unexpected period type {other}"),
        }
    }
}

#[tokio::test]
async fn prior_context_tokens_come_from_the_newest_assistant_message_with_usage() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    // an older answer with other counts, the newest answer with usage, then newer ones that do
    // not count: zero counts and a soft-deleted one
    h.seed_prior_answer(chat, 50, 5).await;
    let rid = Uuid::new_v4();
    h.chat
        .seed_message(
            MessageSeed::user(chat)
                .request(Some(rid))
                .content("q")
                .at(10),
        )
        .await;
    h.chat
        .seed_message(
            MessageSeed::assistant(chat)
                .request(Some(rid))
                .content("a")
                .model(PREMIUM)
                .tokens(600, 287)
                .at(11),
        )
        .await;
    let rid2 = Uuid::new_v4();
    h.chat
        .seed_message(
            MessageSeed::assistant(chat)
                .request(Some(rid2))
                .content("partial")
                .model(PREMIUM)
                .tokens(0, 0)
                .at(12),
        )
        .await;
    let rid3 = Uuid::new_v4();
    h.chat
        .seed_message(
            MessageSeed::assistant(chat)
                .request(Some(rid3))
                .content("gone")
                .model(PREMIUM)
                .tokens(9_000, 9_000)
                .at(13)
                .deleted(),
        )
        .await;

    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let (est_in, max_out, reserve_tokens, credits) =
        ref_reserve(&find_model(&quota_models(), PREMIUM), 32_768, MSG, 887, 0);
    assert_eq!(est_in, 113 + 887);
    assert_eq!(
        turn.reserve_tokens,
        Some(i64::try_from(reserve_tokens).unwrap())
    );
    assert_eq!(
        turn.max_output_tokens_applied,
        Some(i32::try_from(max_out).unwrap())
    );
    assert_eq!(turn.reserved_credits_micro, Some(credits));
}

#[tokio::test]
async fn tool_and_image_surcharges_follow_the_tools_actually_sent() {
    let model = find_model(&quota_models(), PREMIUM);

    // web search requested on a model that supports it: + web_search_surcharge_tokens (500)
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say_web(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let (_, _, tokens, credits) = ref_reserve(&model, 32_768, MSG, 0, 500);
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));

    // the standard model has no web search: no tool, no surcharge (DESIGN 5.5.6)
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat_on(STANDARD).await;
    let sent = h.say_web(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.reserved_credits_micro, Some(standard_credits()));
    assert!(
        turn.web_search_enabled,
        "the flag is stored even when the tool is not sent"
    );

    // a ready document with a vector store: + tool_surcharge_tokens (500)
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_document_with_store(chat).await;
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let (_, _, tokens, credits) = ref_reserve(&model, 32_768, MSG, 0, 500);
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));

    // one image: + image_token_budget (1000)
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let image = h.seed_image(chat).await;
    let sent = h
        .send(chat, &json!({"content": MSG, "attachment_ids": [image]}))
        .await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let (_, _, tokens, credits) = ref_reserve(&model, 32_768, MSG, 0, 1_000);
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));
}

#[tokio::test]
async fn the_output_cap_is_the_smaller_of_the_model_and_the_streaming_limit() {
    let h = QuotaHarness::with_config(json!({"streaming": {"max_output_tokens": 1000}})).await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.max_output_tokens_applied, Some(1000));
    let (_, _, tokens, credits) =
        ref_reserve(&find_model(&quota_models(), PREMIUM), 1000, MSG, 0, 0);
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));
    assert_eq!(
        h.oagw.last_request().body["max_output_tokens"],
        json!(1000),
        "the provider is capped at max_output_tokens_applied"
    );
}

#[tokio::test]
async fn the_minimal_generation_floor_is_capped_by_the_output_cap() {
    // config floor 100 <= streaming cap 100, but the model only generates 60 tokens
    let mut models = quota_models();
    models[0]["max_output_tokens"] = json!(60);
    let h = QuotaHarness::with(QuotaOptions {
        models,
        config: json!({"streaming": {"max_output_tokens": 100},
                       "estimation_budgets": {"minimal_generation_floor": 100}}),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.max_output_tokens_applied, Some(60));
    assert_eq!(
        turn.minimal_generation_floor_applied,
        Some(60),
        "DESIGN 5.5.14: min(config floor, max_output_tokens_applied)"
    );

    // the default floor (50) below the cap is stored as configured
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.minimal_generation_floor_applied, Some(50));
}

#[tokio::test]
async fn the_turn_carries_the_policy_version_it_was_planned_under() {
    let h = QuotaHarness::with(QuotaOptions {
        policy_version: 42,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.policy_version_applied, Some(42));
}

// ═════════════════════════════════════════════════════════════════════════════
// quota rows are upserted: a row conflict is never a 409
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_existing_bucket_row_is_incremented_not_conflicted() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_usage(UsageSeed::total("daily").spent(1_000)).await;
    h.seed_usage(UsageSeed::total("monthly").spent(2_000)).await;
    h.seed_usage(UsageSeed::premium("daily").spent(300)).await;
    h.seed_usage(UsageSeed::premium("monthly").spent(400)).await;

    let sent = h.say(chat, MSG).await;
    assert_eq!(
        sent.names_no_ping().last().unwrap(),
        "done",
        "the quota unique key (tenant, user, period, start, bucket) must be upserted"
    );
    let seen = h.observed.snapshot(0);
    assert_eq!(seen.usage.len(), 4, "no duplicate bucket rows");
    let total_daily = seen
        .usage
        .iter()
        .find(|r| r.bucket == "total" && r.period_type == "daily")
        .unwrap();
    assert_eq!(total_daily.spent_credits_micro, 1_000);
    assert_eq!(total_daily.reserved_credits_micro, premium_credits());
}

#[tokio::test]
async fn concurrent_first_turns_of_one_user_all_succeed_on_a_fresh_bucket() {
    let h = std::sync::Arc::new(QuotaHarness::new().await);
    let mut chats = Vec::new();
    for _ in 0..6 {
        chats.push(h.seed_chat().await);
    }
    let sends = chats.iter().map(|chat| {
        let h = h.clone();
        let chat = *chat;
        async move { h.open(chat, &ctx_a(), &json!({"content": MSG})).await }
    });
    let raws = futures::future::join_all(sends).await;
    let mut statuses = Vec::new();
    let mut streams = Vec::new();
    for raw in raws {
        statuses.push(raw.status.as_u16());
        streams.push(raw);
    }
    assert!(
        statuses.iter().all(|s| *s == 200),
        "no 409 / 429 / 500 from the quota row race: {statuses:?}"
    );
    for raw in streams {
        let sent = raw.collect().await;
        assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    }

    let rows = h.usage_rows().await;
    assert_eq!(rows.len(), 4, "one row per bucket and period: {rows:?}");
    let total = h.bucket("total", "daily").await.unwrap();
    assert_eq!(total.calls, 6);
    assert_eq!(total.reserved_credits_micro, 0);
    let spent = ref_credits(12, 5, 3_000_000, 15_000_000); // the default fake answer: 12 in, 5 out
    assert_eq!(total.spent_credits_micro, 6 * spent);
}

#[tokio::test]
async fn concurrent_requests_cannot_book_more_than_the_limit() {
    let reserve = standard_credits();
    // room for exactly one reserve in the daily total bucket
    let h = std::sync::Arc::new(
        QuotaHarness::with_limits(LimitsSpec::new(
            u64::try_from(reserve).unwrap() + 100,
            10_000_000_000,
            500_000_000,
            5_000_000_000,
        ))
        .await,
    );
    let mut chats = Vec::new();
    for _ in 0..8 {
        chats.push(h.seed_chat_on(STANDARD).await);
    }
    h.oagw
        .set_default(|| ProviderReply::text_with(&["a", "b"], &Opts::default().pause_after(1)));
    let sends = chats.iter().map(|chat| {
        let h = h.clone();
        let chat = *chat;
        async move { h.open(chat, &ctx_a(), &json!({"content": MSG})).await }
    });
    let raws = futures::future::join_all(sends).await;

    let mut admitted = Vec::new();
    for raw in raws {
        if raw.is_sse() {
            admitted.push(raw); // keep the connection open: dropping it would cancel the turn
        } else {
            assert_exhausted(&raw.into_resp().await, "tokens");
        }
    }
    assert_eq!(admitted.len(), 1, "exactly one request is admitted");
    h.oagw.wait_paused(1).await;
    let (spent, reserved) = h.amounts("total", "daily").await;
    assert_eq!(
        (spent, reserved),
        (0, reserve),
        "one reserve, never above the limit"
    );

    h.oagw.release();
    let sent = admitted.pop().unwrap().collect().await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(h.amounts("total", "daily").await.1, 0);
}

// ═════════════════════════════════════════════════════════════════════════════
// tier downgrade
// ═════════════════════════════════════════════════════════════════════════════

/// Run one turn on a premium chat and return the `done` payload and the request id.
async fn done_of(h: &QuotaHarness, chat: Uuid) -> (Value, Uuid) {
    let sent = h.say(chat, MSG).await;
    sent.assert_well_formed();
    assert_eq!(
        sent.names_no_ping().last().unwrap(),
        "done",
        "{:?}",
        sent.events
    );
    (sent.done().clone(), sent.request_id())
}

#[tokio::test]
async fn an_exhausted_premium_tier_runs_on_the_standard_model() {
    for period in ["daily", "monthly"] {
        let h = QuotaHarness::with_limits(LimitsSpec::new(
            1_000_000_000,
            10_000_000_000,
            500_000,
            5_000_000,
        ))
        .await;
        let chat = h.seed_chat().await;
        let limit = if period == "daily" {
            500_000
        } else {
            5_000_000
        };
        h.seed_usage(UsageSeed::premium(period).spent(limit)).await;
        h.oagw.push(answer_with_usage(20, 10));

        let (done, rid) = done_of(&h, chat).await;

        assert_eq!(done["quota_decision"], json!("downgrade"), "{period}");
        assert_eq!(done["selected_model"], json!(PREMIUM));
        assert_eq!(done["effective_model"], json!(STANDARD));
        assert_eq!(done["downgrade_from"], json!(PREMIUM));
        assert_eq!(done["downgrade_reason"], json!("premium_quota_exhausted"));

        // the provider is called with the downgraded model's provider_model_id
        let request = h.oagw.last_request();
        assert_eq!(request.body["model"], json!(provider_model(STANDARD)));
        // the effective model is persisted on the turn and the assistant message
        let turn = h.turn_of(rid).await.unwrap();
        assert_eq!(turn.effective_model.as_deref(), Some(STANDARD));
        let messages = h.chat.all_messages().await;
        let assistant = messages.iter().find(|m| m.role == "assistant").unwrap();
        assert_eq!(assistant.model.as_deref(), Some(STANDARD));
        assert_eq!(
            h.chat.chat_row(chat).await.unwrap().model,
            PREMIUM,
            "the chat keeps its selected model"
        );
        // the booked reserve is the standard candidate's, on the total bucket only
        assert_eq!(turn.reserved_credits_micro, Some(standard_credits()));
        let seen = h.observed.snapshot(0);
        let total = seen
            .usage
            .iter()
            .find(|r| r.bucket == "total" && r.period_type == "daily")
            .unwrap();
        assert_eq!(total.reserved_credits_micro, standard_credits());
        let premium = h.bucket("tier:premium", period).await.unwrap();
        assert_eq!(
            (
                premium.spent_credits_micro,
                premium.reserved_credits_micro,
                premium.calls
            ),
            (limit, 0, 0),
            "a standard turn leaves the premium sub-cap untouched"
        );
    }
}

#[tokio::test]
async fn a_shared_cap_that_fits_standard_but_not_premium_downgrades_too() {
    // 30_000 daily in total: the premium reserve (61_779) does not fit, the standard one (12_401) does
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        30_000,
        10_000_000_000,
        500_000_000,
        5_000_000_000,
    ))
    .await;
    let chat = h.seed_chat().await;
    let (done, _) = done_of(&h, chat).await;
    assert_eq!(done["effective_model"], json!(STANDARD));
    assert_eq!(done["downgrade_reason"], json!("premium_quota_exhausted"));
}

#[tokio::test]
async fn the_kill_switches_downgrade_with_their_own_reason() {
    for (switch, reason) in [
        ("force_standard_tier", "force_standard_tier"),
        ("disable_premium_tier", "disable_premium_tier"),
    ] {
        let h = QuotaHarness::with_limits(roomy()).await;
        h.policy.set_kill_switches(json!({ switch: true }));
        let chat = h.seed_chat().await;
        let (done, rid) = done_of(&h, chat).await;
        assert_eq!(done["quota_decision"], json!("downgrade"), "{switch}");
        assert_eq!(done["downgrade_from"], json!(PREMIUM));
        assert_eq!(done["downgrade_reason"], json!(reason));
        assert_eq!(done["effective_model"], json!(STANDARD));
        assert_eq!(
            h.turn_of(rid).await.unwrap().effective_model.as_deref(),
            Some(STANDARD)
        );
        assert_eq!(
            h.oagw.last_request().body["model"],
            json!(provider_model(STANDARD))
        );
    }
}

#[tokio::test]
async fn the_premium_kill_switches_do_not_touch_a_standard_chat() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true, "disable_premium_tier": true}));
    let chat = h.seed_chat_on(STANDARD).await;
    let (done, _) = done_of(&h, chat).await;
    assert_eq!(done["quota_decision"], json!("allow"));
    assert_eq!(done["effective_model"], json!(STANDARD));
    assert!(done.get("downgrade_reason").is_none_or(Value::is_null));
    assert!(done.get("downgrade_from").is_none_or(Value::is_null));
}

#[tokio::test]
async fn a_kill_switch_does_not_bypass_an_exhausted_shared_cap() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    h.seed_usage(UsageSeed::total("monthly").spent(10_000_000))
        .await;
    let chat = h.seed_chat().await;
    let resp = h.reject(chat, json!({"content": MSG})).await;
    assert_exhausted(&resp, "tokens");
}

#[tokio::test]
async fn a_disabled_selected_model_runs_on_an_enabled_model_with_model_disabled() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat_on(DISABLED).await;
    let (done, rid) = done_of(&h, chat).await;
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["selected_model"], json!(DISABLED));
    assert_eq!(done["downgrade_from"], json!(DISABLED));
    assert_eq!(done["downgrade_reason"], json!("model_disabled"));
    assert_eq!(done["effective_model"], json!(STANDARD));
    assert_eq!(
        h.turn_of(rid).await.unwrap().effective_model.as_deref(),
        Some(STANDARD)
    );
}

#[tokio::test]
async fn the_downgrade_candidate_is_the_tenant_default_of_the_tier() {
    let mut models = quota_models();
    models.push(standard_two()); // enabled standard, preference.is_default = true
    let h = QuotaHarness::with(QuotaOptions {
        models,
        limits: roomy(),
        ..Default::default()
    })
    .await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let chat = h.seed_chat().await;
    let (done, rid) = done_of(&h, chat).await;
    assert_eq!(done["effective_model"], json!(quota_support::STANDARD_2));
    assert_eq!(
        h.oagw.last_request().body["model"],
        json!(provider_model(quota_support::STANDARD_2))
    );
    let turn = h.turn_of(rid).await.unwrap();
    let two = standard_two();
    let (_, _, tokens, credits) = ref_reserve(&two, 32_768, MSG, 0, 0);
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(
        turn.reserved_credits_micro,
        Some(credits),
        "the candidate's own reserve"
    );
}

#[tokio::test]
async fn an_unchanged_selection_is_allow_without_downgrade_fields() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    let (done, rid) = done_of(&h, chat).await;
    assert_eq!(done["quota_decision"], json!("allow"));
    assert_eq!(done["effective_model"], json!(PREMIUM));
    assert_eq!(done["selected_model"], json!(PREMIUM));
    assert!(done.get("downgrade_from").is_none_or(Value::is_null));
    assert!(done.get("downgrade_reason").is_none_or(Value::is_null));
    assert_eq!(
        h.oagw.last_request().body["model"],
        json!(provider_model(PREMIUM))
    );
    assert_eq!(
        h.turn_of(rid).await.unwrap().effective_model.as_deref(),
        Some(PREMIUM)
    );
}

// ── vision is checked against the effective model ───────────────────────────

#[tokio::test]
async fn images_after_a_downgrade_to_a_model_without_vision_are_rejected_before_any_call() {
    let h = QuotaHarness::with_limits(LimitsSpec::new(
        1_000_000_000,
        10_000_000_000,
        500_000,
        5_000_000,
    ))
    .await;
    let chat = h.seed_chat().await; // premium has VISION_INPUT, standard has not
    let image = h.seed_image(chat).await;
    h.seed_usage(UsageSeed::premium("daily").spent(500_000))
        .await;
    let before = h.row_counts().await;

    let resp = h
        .reject(chat, json!({"content": MSG, "attachment_ids": [image]}))
        .await;

    let problem = assert_400_reason(&resp, "invalid_argument", "VISION_NOT_SUPPORTED");
    assert_field_violation(&problem, "content_type", "VISION_NOT_SUPPORTED");
    h.oagw.assert_no_provider_call();
    assert_eq!(
        h.row_counts().await,
        before,
        "no turn, message or reserve is left behind"
    );
    assert_eq!(h.amounts("total", "daily").await, (0, 0));
}

#[tokio::test]
async fn images_on_a_model_with_vision_and_a_downgrade_without_images_proceed() {
    // premium available + image: fine
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    let image = h.seed_image(chat).await;
    let sent = h
        .send(chat, &json!({"content": MSG, "attachment_ids": [image]}))
        .await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    // downgraded to the model without vision but no image: fine
    let h = QuotaHarness::with_limits(roomy()).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let chat = h.seed_chat().await;
    let (done, _) = done_of(&h, chat).await;
    assert_eq!(done["effective_model"], json!(STANDARD));
}

#[tokio::test]
async fn a_selected_standard_model_without_vision_rejects_images_too() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat_on(STANDARD).await;
    let image = h.seed_image(chat).await;
    let resp = h
        .reject(chat, json!({"content": MSG, "attachment_ids": [image]}))
        .await;
    assert_400_reason(&resp, "invalid_argument", "VISION_NOT_SUPPORTED");
    h.oagw.assert_no_provider_call();
}
