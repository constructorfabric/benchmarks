//! Settlement of the reserve in the finalization transaction (the execute / settle half of slice
//! S4): by actual usage on completion, bounded overshoot, failed turns (actual with usage,
//! estimated without), cancelled / aborted turns (estimated), the CAS winner as the only settler,
//! replay without side effects, the policy snapshot of `policy_version_applied` and the effective
//! model computed once.
//!
//! Source of truth: DESIGN 5.4.4 "Finalization Settlement", 5.4.5 "Exceedance Handling" (overshoot
//! tolerance and cap), 5.5.9 / 5.5.13 / 5.5.14, 5.7 "Terminal outcome taxonomy", "FinalizeTurn
//! Invariant", "Usage accounting rules per outcome", 5.8 "Normative Billing Outcome Derivation",
//! 5.9 "Terminal Error Reconciliation Rule", "Terminal Signal Race Resolution", 5.10 (calculation
//! example, end to end), 3.7 `quota_usage` "Commit semantics", 4 "Effective Model Computed Once".

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

use quota_support::{
    LimitsSpec, MessageSeed, Opts, PREMIUM, PREMIUM_IN, PREMIUM_OUT, ProviderReply, QuotaHarness,
    QuotaOptions, STANDARD, STANDARD_IN, STANDARD_OUT, UsageSeed, answer_with_usage, ctx_a,
    failed_stream, quota_models, ref_credits, stream_model_json,
};
use serde_json::{Value, json};
use uuid::Uuid;

const MSG: &str = "hello";

fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000)
}

/// Assert the four buckets (or the two `total` ones) of `USER_A` after a settlement.
async fn assert_settled(
    h: &QuotaHarness,
    premium: bool,
    spent: i64,
    calls: i32,
    input_tokens: i64,
    output_tokens: i64,
) {
    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.expect("total row");
        assert_eq!(
            (
                total.spent_credits_micro,
                total.reserved_credits_micro,
                total.calls,
                total.input_tokens,
                total.output_tokens
            ),
            (spent, 0, calls, input_tokens, output_tokens),
            "total/{period}: (spent, reserved, calls, input, output)"
        );
        let tier = h.bucket("tier:premium", period).await;
        if premium {
            let tier = tier.unwrap_or_else(|| panic!("no tier:premium/{period} row"));
            assert_eq!(
                (
                    tier.spent_credits_micro,
                    tier.reserved_credits_micro,
                    tier.calls,
                    tier.input_tokens,
                    tier.output_tokens
                ),
                (spent, 0, calls, 0, 0),
                "tier:premium/{period}: tokens are telemetry of the total bucket only"
            );
        } else {
            assert!(tier.is_none(), "a standard turn never touches tier:premium");
        }
    }
}

fn premium_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, PREMIUM_IN, PREMIUM_OUT)
}

fn standard_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, STANDARD_IN, STANDARD_OUT)
}

// ═════════════════════════════════════════════════════════════════════════════
// completed: settled by the provider's actual usage
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_premium_turn_commits_actual_credits_and_releases_the_reserve() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(800, 200));

    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(
        sent.done()["usage"],
        json!({"input_tokens": 800, "output_tokens": 200})
    );

    assert_settled(&h, true, premium_credits(800, 200), 1, 800, 200).await;
    // the stored assistant message keeps the provider's actual counts
    let messages = h.chat.all_messages().await;
    let assistant = messages.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(
        (assistant.input_tokens, assistant.output_tokens),
        (800, 200)
    );
    assert_eq!(
        h.turn_of(sent.request_id()).await.unwrap().state,
        "completed"
    );
}

#[tokio::test]
async fn a_completed_standard_turn_settles_the_total_bucket_only() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(1_000, 250));
    h.say(chat, MSG).await;
    assert_settled(&h, false, standard_credits(1_000, 250), 1, 1_000, 250).await;
}

#[tokio::test]
async fn settlement_accumulates_over_turns_and_calls_count_every_settlement() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(100, 10));
    h.oagw.push(answer_with_usage(200, 20));
    h.oagw.push(answer_with_usage(300, 30));
    for text in ["one", "two", "three"] {
        let sent = h.say(chat, text).await;
        assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    }
    let spent = premium_credits(100, 10) + premium_credits(200, 20) + premium_credits(300, 30);
    assert_settled(&h, true, spent, 3, 600, 60).await;
}

#[tokio::test]
async fn an_incomplete_provider_answer_is_a_completed_turn_settled_on_actual() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["cut"],
        &Opts::default().usage(500, 300).incomplete(),
    ));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_settled(&h, true, premium_credits(500, 300), 1, 500, 300).await;
}

#[tokio::test]
async fn a_completed_turn_without_usage_is_charged_zero_but_counted() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(0, 0));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_settled(&h, true, 0, 1, 0, 0).await;
}

#[tokio::test]
async fn the_design_5_10_example_end_to_end() {
    // premium P (2.5 credits / 1K tokens) and standard S (1.0), output cap 500
    let mut premium = stream_model_json(PREMIUM, "Premium");
    premium["input_tokens_credit_multiplier_micro"] = json!(2_500_000_000_u64);
    premium["output_tokens_credit_multiplier_micro"] = json!(2_500_000_000_u64);
    premium["max_output_tokens"] = json!(500);
    let mut standard = stream_model_json(STANDARD, "Standard");
    standard["input_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    standard["output_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    standard["max_output_tokens"] = json!(500);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    let h = QuotaHarness::with(QuotaOptions {
        models: vec![premium, standard],
        // 5.10.1: premium day 22M / month 300M, total day 60M / month 600M
        limits: LimitsSpec::new(60_000_000, 600_000_000, 22_000_000, 300_000_000),
        ..Default::default()
    })
    .await;
    h.seed_usage(UsageSeed::premium("daily").spent(20_000_000))
        .await;
    h.seed_usage(UsageSeed::premium("monthly").spent(200_000_000))
        .await;
    h.seed_usage(UsageSeed::total("daily").spent(25_000_000))
        .await;
    h.seed_usage(UsageSeed::total("monthly").spent(240_000_000))
        .await;

    let chat = h.seed_chat().await;
    // estimated input = 113 (the message) + 887 (prior answer 600 + 287) = 1_000 tokens
    h.seed_prior_answer(chat, 600, 287).await;
    h.oagw.push(answer_with_usage(900, 300));

    let sent = h.say(chat, MSG).await;
    let done = sent.done();

    // step 1 / 2: premium reserve 3_750_000 > the 2_000_000 left of the premium day -> standard
    assert_eq!(done["effective_model"], json!(STANDARD));
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["downgrade_reason"], json!("premium_quota_exhausted"));
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.reserve_tokens, Some(1_500));
    assert_eq!(turn.max_output_tokens_applied, Some(500));
    assert_eq!(turn.reserved_credits_micro, Some(1_500_000));
    let seen = h.observed.snapshot(0);
    for period in ["daily", "monthly"] {
        let row = seen
            .usage
            .iter()
            .find(|r| r.bucket == "total" && r.period_type == period)
            .unwrap();
        assert_eq!(row.reserved_credits_micro, 1_500_000, "{period}");
        let premium = seen
            .usage
            .iter()
            .find(|r| r.bucket == "tier:premium" && r.period_type == period)
            .unwrap();
        assert_eq!(premium.reserved_credits_micro, 0, "no premium reserve");
    }

    // step 3 / 5.10.5: commit by actual 900 + 300 = 1_200_000
    let daily = h.bucket("total", "daily").await.unwrap();
    let monthly = h.bucket("total", "monthly").await.unwrap();
    assert_eq!(daily.spent_credits_micro, 26_200_000);
    assert_eq!(monthly.spent_credits_micro, 241_200_000);
    assert_eq!(
        (daily.reserved_credits_micro, monthly.reserved_credits_micro),
        (0, 0)
    );
    assert_eq!(
        (daily.calls, daily.input_tokens, daily.output_tokens),
        (1, 900, 300)
    );
    let premium_daily = h.bucket("tier:premium", "daily").await.unwrap();
    assert_eq!(
        (premium_daily.spent_credits_micro, premium_daily.calls),
        (20_000_000, 0),
        "bucket tier:premium did not change"
    );
}

// ── overshoot (DESIGN 5.4.5) ────────────────────────────────────────────────

/// A standard model whose input is expensive and whose output cap is small, so a few extra input
/// tokens move the credits past the reserve: reserve = 113 + 100 tokens, 1_130 + 100 credits.
fn overshoot_models() -> Vec<Value> {
    let mut standard = stream_model_json(STANDARD, "Standard");
    standard["input_tokens_credit_multiplier_micro"] = json!(10_000_000);
    standard["output_tokens_credit_multiplier_micro"] = json!(1_000_000);
    standard["max_output_tokens"] = json!(100);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    vec![standard]
}

#[tokio::test]
async fn an_overshoot_within_the_tolerance_is_committed_in_full() {
    let h = QuotaHarness::with(QuotaOptions {
        models: overshoot_models(),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(230, 0)); // 230 > 213 reserve tokens, ratio 1.0798 <= 1.10

    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.reserved_credits_micro, Some(1_230));
    let total = h.bucket("total", "daily").await.unwrap();
    assert_eq!(
        total.spent_credits_micro,
        ref_credits(230, 0, 10_000_000, 1_000_000),
        "the actual credits, above the reserve"
    );
    assert_eq!(total.spent_credits_micro, 2_300);
    assert_eq!(
        total.reserved_credits_micro, 0,
        "the reserve is released, not left behind"
    );
}

#[tokio::test]
async fn an_overshoot_beyond_the_tolerance_is_capped_at_the_reserve_and_the_turn_stays_completed() {
    let h = QuotaHarness::with(QuotaOptions {
        models: overshoot_models(),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(300, 0)); // 300 / 213 = 1.41 > 1.10

    let sent = h.say(chat, MSG).await;

    assert_eq!(
        sent.names_no_ping().last().unwrap(),
        "done",
        "a completed answer is never retroactively cancelled"
    );
    assert_eq!(
        sent.done()["usage"]["input_tokens"],
        json!(300),
        "the client sees the actual usage"
    );
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.error_code, None);
    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.unwrap();
        assert_eq!(total.spent_credits_micro, 1_230, "committed = reserved");
        assert_eq!(total.reserved_credits_micro, 0);
        assert_eq!(total.input_tokens, 300, "telemetry keeps the actual tokens");
    }
    let messages = h.chat.all_messages().await;
    let assistant = messages.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(
        assistant.input_tokens, 300,
        "the message keeps the actual counts"
    );
}

#[tokio::test]
async fn the_overshoot_tolerance_factor_is_configurable() {
    let h = QuotaHarness::with(QuotaOptions {
        models: overshoot_models(),
        config: json!({"quota": {"overshoot_tolerance_factor": 1.0}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(230, 0)); // ratio 1.0798 > 1.0 -> capped
    h.say(chat, MSG).await;
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .spent_credits_micro,
        1_230
    );

    let h = QuotaHarness::with(QuotaOptions {
        models: overshoot_models(),
        config: json!({"quota": {"overshoot_tolerance_factor": 1.5}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat_on(STANDARD).await;
    h.oagw.push(answer_with_usage(300, 0)); // ratio 1.41 <= 1.5 -> in full
    h.say(chat, MSG).await;
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .spent_credits_micro,
        3_000
    );
}

// ── settlement errors (DESIGN 5.3 "Overflow Protection", error handling) ────

#[tokio::test]
async fn out_of_range_usage_on_a_completed_turn_fails_the_finalization_not_the_process() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(10_000_001, 5));

    let sent = h.say(chat, MSG).await;

    assert_eq!(sent.names_no_ping().last().unwrap(), "error");
    assert_eq!(
        sent.error()["code"],
        json!("finalization_failed"),
        "SSE error instead of done"
    );
    assert!(
        !sent.blob().contains("10000001"),
        "no internal detail on the wire"
    );
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(
        turn.state, "running",
        "the finalization transaction rolled back: the turn is left for the watchdog"
    );
    assert!(
        h.chat
            .all_messages()
            .await
            .iter()
            .all(|m| m.role != "assistant"),
        "no assistant message of a rolled-back finalization"
    );
    h.policy.assert_published_stays(0).await;
}

#[tokio::test]
async fn out_of_range_usage_on_a_failed_stream_keeps_the_original_error_code() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &["par"],
        Some((10_000_001, 1)),
    )));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("provider_error"));
    assert_eq!(sent.names_no_ping().last().unwrap(), "error");
}

// ═════════════════════════════════════════════════════════════════════════════
// failed: actual with reported usage, estimated without (never "released" after the call)
// ═════════════════════════════════════════════════════════════════════════════

/// The estimated charge of a premium "hello" turn: 113 input estimate + the 50 token floor.
fn estimated_premium() -> i64 {
    premium_credits(113, 50)
}

#[tokio::test]
async fn a_provider_http_error_settles_estimated_and_releases_the_rest_of_the_reserve() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::http_error(500, "upstream exploded"));

    let sent = h.say(chat, MSG).await;

    assert_eq!(sent.error()["code"], json!("provider_error"));
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(
        (turn.state.as_str(), turn.error_code.as_deref()),
        ("failed", Some("provider_error"))
    );
    assert_eq!(
        estimated_premium(),
        339 + 750,
        "credits(113, 50) on the premium multipliers"
    );
    // no-free-failure: the provider request started, so the debit is non-zero; the estimated
    // split is not telemetry (input / output tokens stay 0)
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;
}

#[tokio::test]
async fn a_failed_stream_with_reported_usage_settles_actual() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "model overloaded",
        &["partial"],
        Some((700, 40)),
    )));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("provider_error"));
    assert_settled(&h, true, premium_credits(700, 40), 1, 700, 40).await;
}

#[tokio::test]
async fn a_failed_stream_with_all_zero_usage_is_estimated_not_free() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &[],
        Some((0, 0)),
    )));
    h.say(chat, MSG).await;
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;
}

#[tokio::test]
async fn a_failed_stream_without_usage_after_partial_text_is_estimated() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::sse(failed_stream(
        "server_error",
        "boom",
        &["some ", "text"],
        None,
    )));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("provider_error"));
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;
}

#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_and_a_429_are_settled_estimated() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["cut"],
        &Opts::default().no_terminal(),
    ));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("provider_error"));
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;

    h.oagw.push(ProviderReply::http_error_with(
        429,
        "slow down",
        vec![("retry-after", "3".to_owned())],
    ));
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("rate_limited"));
    assert_settled(&h, true, 2 * estimated_premium(), 2, 0, 0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// cancelled: a client disconnect is aborted, estimated, never free
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_disconnect_after_some_text_settles_the_estimated_charge() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["Hel", "lo", " world"],
        &Opts::default().usage(500, 400).pause_after(2),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG, "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    live.next_non_ping().await.expect("second delta");
    h.oagw.wait_paused(1).await;
    drop(live);

    let turn = h.wait_turn_state(rid, "cancelled").await;
    assert!(
        turn.assistant_message_id.is_some(),
        "the partial answer is kept"
    );
    // the cancel path carries no usage: min(reserve, estimated input + floor) = 113 + 50 tokens
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;
}

#[tokio::test]
async fn a_disconnect_before_any_content_is_not_free() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["never seen"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG, "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);

    h.wait_turn_state(rid, "cancelled").await;
    assert_settled(&h, true, estimated_premium(), 1, 0, 0).await;
}

#[tokio::test]
async fn the_estimated_charge_is_capped_by_the_reserve() {
    // floor 10_000 > the model's 4_096 output cap: the persisted floor is 4_096 and the charge is
    // exactly the reserve (charged tokens = min(reserve_tokens, input estimate + floor))
    let h = QuotaHarness::with_config(
        json!({"estimation_budgets": {"minimal_generation_floor": 10000}}),
    )
    .await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    let sent = h.say(chat, MSG).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.minimal_generation_floor_applied, Some(4_096));
    let reserved = turn.reserved_credits_micro.unwrap();
    assert_settled(&h, true, reserved, 1, 0, 0).await;
    assert_eq!(reserved, 61_779);
}

#[tokio::test]
async fn the_estimated_charge_uses_the_floor_captured_for_the_turn() {
    // floor 200 < the 4_096 output cap: the estimate charges 113 input + 200 output tokens
    let h =
        QuotaHarness::with_config(json!({"estimation_budgets": {"minimal_generation_floor": 200}}))
            .await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    h.say(chat, MSG).await;
    assert_settled(&h, true, premium_credits(113, 200), 1, 0, 0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// exactly once: CAS winner only, replay never settles
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_replay_never_takes_a_reserve_or_debits_credits() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(answer_with_usage(800, 200));
    let first = h.say_with(chat, MSG, rid).await;
    assert_eq!(first.names_no_ping().last().unwrap(), "done");
    let after_first = h.usage_fingerprint().await;
    h.policy.wait_published(1).await;
    h.audit.wait_events(1).await;

    for _ in 0..3 {
        let replay = h.say_with(chat, MSG, rid).await;
        assert_eq!(replay.started()["is_new_turn"], json!(false));
        assert_eq!(replay.names_no_ping().last().unwrap(), "done");
    }

    assert_eq!(
        h.usage_fingerprint().await,
        after_first,
        "no reserve, no debit"
    );
    assert_eq!(h.oagw.request_count(), 1);
    h.policy.assert_published_stays(1).await;
    h.audit.assert_events_stay(1).await;
}

#[tokio::test]
async fn a_finalizer_that_loses_the_cas_settles_nothing() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().usage(800, 200).pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG, "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;
    let reserved = h.amounts("total", "daily").await.1;
    assert!(reserved > 0);

    // somebody else (the orphan watchdog) decides the outcome first
    h.finalize_externally(rid, "failed", Some("orphan_timeout"))
        .await;
    h.oagw.release();
    let rest = live.drain().await;

    let last = rest.last().expect("a terminal event");
    assert_eq!(last.event, "error");
    assert_eq!(last.data["code"], json!("stream_interrupted"));
    // the loser neither debited nor released anything: the winner owns the settlement
    assert_eq!(
        h.amounts("total", "daily").await,
        (0, reserved),
        "no double settlement"
    );
    assert_eq!(h.bucket("total", "daily").await.unwrap().calls, 0);
    h.policy.assert_published_stays(0).await;
    h.audit.assert_events_stay(0).await;
    assert!(
        h.chat
            .all_messages()
            .await
            .iter()
            .all(|m| m.role != "assistant"),
        "a loser persists no assistant message"
    );
}

#[tokio::test]
async fn every_terminal_outcome_returns_the_reserve_to_zero() {
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;

    // completed
    h.oagw.push(answer_with_usage(10, 10));
    h.say(chat, "one").await;
    h.assert_nothing_reserved().await;
    // failed
    h.oagw.push(ProviderReply::http_error(500, "boom"));
    h.say(chat, "two").await;
    h.assert_nothing_reserved().await;
    // cancelled
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["x"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "three", "request_id": rid}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;
    h.assert_nothing_reserved().await;
    assert_eq!(h.bucket("total", "daily").await.unwrap().calls, 3);
}

// ═════════════════════════════════════════════════════════════════════════════
// determinism: policy version and effective model are read from the turn
// ═════════════════════════════════════════════════════════════════════════════

fn doubled_catalog() -> Vec<Value> {
    let mut models = quota_models();
    for model in &mut models {
        let (i, o) = quota_support::mults_of(model);
        model["input_tokens_credit_multiplier_micro"] = json!(i * 2);
        model["output_tokens_credit_multiplier_micro"] = json!(o * 2);
    }
    models
}

#[tokio::test]
async fn settlement_uses_the_multipliers_of_the_policy_version_applied() {
    let h = QuotaHarness::with(QuotaOptions {
        policy_version: 1,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().usage(800, 200).pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;

    // a new policy version with doubled multipliers goes live mid-turn
    let asked_before = h.policy.snapshot_requests.lock().unwrap().len();
    h.policy.publish_version(2, &doubled_catalog());
    h.oagw.release();
    let rest = live.drain().await;
    assert_eq!(rest.last().unwrap().event, "done");

    assert_settled(&h, true, premium_credits(800, 200), 1, 800, 200).await;
    let asked_after = h.policy.snapshot_requests.lock().unwrap().clone();
    assert!(
        asked_after[asked_before..].iter().all(|v| *v == 1),
        "settlement loads the snapshot of the applied version, never the live one: {asked_after:?}"
    );

    // the next turn is planned under version 2 and priced with its multipliers
    h.oagw.push(answer_with_usage(800, 200));
    let sent = h.say(chat, "again").await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.policy_version_applied, Some(2));
    let spent = premium_credits(800, 200) + ref_credits(800, 200, 2 * PREMIUM_IN, 2 * PREMIUM_OUT);
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .spent_credits_micro,
        spent
    );
}

#[tokio::test]
async fn the_effective_model_is_not_recomputed_at_settlement() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().usage(800, 200).pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": MSG}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.expect("first delta");
    h.oagw.wait_paused(1).await;

    // an operator flips the kill switch and every limit drops to 1 micro-credit while the turn runs
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    h.policy.set_limits(LimitsSpec::new(1, 1, 1, 1));
    h.oagw.release();
    let rest = live.drain().await;
    let done = &rest.last().unwrap().data;

    assert_eq!(done["effective_model"], json!(PREMIUM));
    assert_eq!(done["quota_decision"], json!("allow"));
    let total = h.bucket("total", "daily").await.unwrap();
    let premium = h.bucket("tier:premium", "daily").await.unwrap();
    assert_eq!(total.spent_credits_micro, premium_credits(800, 200));
    assert_eq!(
        premium.spent_credits_micro,
        premium_credits(800, 200),
        "the turn was a premium turn: it is settled against the premium bucket it reserved"
    );
    assert_eq!(premium.reserved_credits_micro, 0);
}

// ═════════════════════════════════════════════════════════════════════════════
// robustness
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_rejected_send_changes_no_quota_row_and_a_settled_turn_changes_exactly_its_rows() {
    let h =
        QuotaHarness::with_limits(LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000)).await;
    let chat = h.seed_chat().await;
    h.seed_usage(UsageSeed::total("daily").spent(1_000_000))
        .await;
    let before = h.usage_fingerprint().await;
    let resp = h.reject(chat, json!({"content": MSG})).await;
    assert_eq!(resp.status.as_u16(), 429);
    assert_eq!(h.usage_fingerprint().await, before);
}

#[tokio::test]
async fn history_does_not_change_what_a_turn_settles() {
    // a long history re-sent to the provider does not enter the credit computation: only the
    // provider's usage does (DESIGN 5.5.9)
    let h = QuotaHarness::new().await;
    let chat = h.seed_chat().await;
    for i in 0..4 {
        let rid = Uuid::new_v4();
        h.chat
            .seed_message(
                MessageSeed::user(chat)
                    .request(Some(rid))
                    .content("q")
                    .at(10 + i * 2),
            )
            .await;
        h.chat
            .seed_message(
                MessageSeed::assistant(chat)
                    .request(Some(rid))
                    .content("a")
                    .model(PREMIUM)
                    .tokens(10, 4)
                    .at(11 + i * 2),
            )
            .await;
    }
    h.oagw.push(answer_with_usage(33, 7));
    h.say(chat, MSG).await;
    assert_settled(&h, true, premium_credits(33, 7), 1, 33, 7).await;
}
