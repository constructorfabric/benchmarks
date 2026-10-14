//! The summary provider call over a scripted transport and a virtual timer: the request the
//! transport receives, the single call within the time left (a hung call ends at the deadline, an
//! attempt without time left makes no call), the fixed causes of a transport or gateway failure and
//! of a malformed or oversized body, the prompt-too-long retries (at most two, the oldest fifth
//! dropped each time with at least two messages kept, one shared time bound for all attempts, no
//! usage from rejected attempts) and the rule that no other failure is retried by this algorithm.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithms
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call` (`inst-call-build`,
//! `inst-call-deadline`, `inst-call-try`, `inst-call-post`, `inst-call-catch-ptl`,
//! `inst-call-ptl-retry`, `inst-call-ptl-exhausted`, `inst-call-catch-other`,
//! `inst-call-return-failure`, `inst-call-return`) and
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call` (`inst-ad-no-time`,
//! `inst-ad-return-no-time`, `inst-ad-post`, `inst-ad-transport-failed`, `inst-ad-return-transport`,
//! `inst-ad-return-other`, `inst-ad-return-ptl`, `inst-ad-return`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution` and
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{ModelCatalogEntry, UsageTokens};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ProviderCall, SummaryCaller, TransportFailure, call_once};
use crate::config::ProviderKind;
use crate::domain::attachments::timing::Timer;
use crate::domain::maintenance::settings::SummaryWorkerSettings;
use crate::domain::maintenance::summary_prompt::{
    RangeMessage, SummaryPrompt, build_prompt, drop_oldest_fifth, render_user_prompt,
};
use crate::domain::maintenance::summary_request::{
    SummaryRequest, SummaryRequestInput, build_summary_request,
};
use crate::domain::maintenance::summary_response::{SummaryCallFailure, SummaryCompletion};
use crate::domain::provider_registry::EffectiveProvider;
use crate::test_support::TENANT_A;
use crate::test_support::maintenance::{
    SUMMARY_MODEL, Scripted, ScriptedTransport, catalog, context_length_error, providers, range_of,
    responses_answer, settings_for, summary_text,
};
use crate::test_support::manual_time::ManualTime;
use crate::test_support::quota::default_now;

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_000A);
const MALFORMED: &str = "the provider response is malformed";
const NO_TIME: &str = "the provider phase deadline was reached";

fn good(usage: (u64, u64)) -> Scripted {
    Scripted::ok(&responses_answer(&summary_text("S"), usage))
}

fn too_long() -> Scripted {
    Scripted::status(400, &context_length_error())
}

fn model() -> ModelCatalogEntry {
    catalog()
        .model_catalog
        .into_iter()
        .find(|entry| entry.id == SUMMARY_MODEL)
        .unwrap()
}

fn provider() -> EffectiveProvider {
    providers()
        .resolve("azure_openai", TENANT_A)
        .unwrap()
        .clone()
}

fn request() -> SummaryRequest {
    let provider = provider();
    let model = model();
    build_summary_request(&SummaryRequestInput {
        provider: &provider,
        model: &model,
        tenant_id: TENANT_A,
        chat_id: CHAT,
        system_prompt: "SYS",
        user_prompt: "USER",
    })
}

struct Rig {
    time: Arc<ManualTime>,
    transport: Arc<ScriptedTransport>,
    caller: SummaryCaller,
}

fn rig(default: Scripted) -> Rig {
    let time = ManualTime::auto(default_now());
    let transport = ScriptedTransport::answering(&time, default);
    let caller = SummaryCaller {
        transport: transport.clone(),
        timer: time.clone(),
    };
    Rig {
        time,
        transport,
        caller,
    }
}

fn is_other(result: &Result<SummaryCompletion, SummaryCallFailure>, cause: &str) -> bool {
    matches!(result, Err(SummaryCallFailure::Other { cause: found }) if found == cause)
}

// ── one attempt ──────────────────────────────────────────────────────────────

async fn once(
    rig: &Rig,
    kind: ProviderKind,
    time_left: Duration,
) -> Result<SummaryCompletion, SummaryCallFailure> {
    call_once(
        rig.transport.as_ref(),
        rig.time.as_ref(),
        &request(),
        kind,
        time_left,
    )
    .await
}

#[tokio::test]
async fn the_transport_receives_the_uri_and_the_body_of_the_request() {
    // inst-ad-post: one `POST` through the transport, without retry
    let rig = rig(good((10, 5)));
    let expected = request();
    let completion = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60))
        .await
        .expect("success");
    let calls = rig.transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].uri, expected.uri);
    assert_eq!(calls[0].body, expected.body);
    assert_eq!(completion.text, summary_text("S"));
    assert_eq!(
        completion.usage,
        UsageTokens {
            input_tokens: 10,
            output_tokens: 5,
            ..UsageTokens::default()
        }
    );
}

#[tokio::test]
async fn no_time_left_fails_without_a_call() {
    // inst-ad-no-time, inst-ad-return-no-time
    let rig = rig(good((1, 1)));
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::ZERO).await;
    assert!(is_other(&result, NO_TIME), "{result:?}");
    assert_eq!(rig.transport.call_count(), 0);
}

#[tokio::test]
async fn a_hung_call_ends_when_the_time_left_is_used_up() {
    // inst-ad-post: ending the call when the time left is used up; inst-ad-transport-failed
    let rig = rig(Scripted::Hang);
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(7)).await;
    assert!(
        matches!(result, Err(SummaryCallFailure::Other { .. })),
        "{result:?}"
    );
    assert_eq!(rig.time.elapsed(), Duration::from_secs(7));
    assert_eq!(rig.transport.call_count(), 1);
}

#[tokio::test]
async fn a_call_that_answers_within_the_time_left_is_not_cut() {
    // inst-ad-post: the call is polled first, so an answer inside the bound wins
    let rig = rig(Scripted::takes(Duration::from_secs(3), good((1, 1))));
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(5)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(rig.time.elapsed(), Duration::from_secs(3));
}

#[tokio::test]
async fn a_transport_or_gateway_failure_has_a_fixed_cause_naming_only_the_kind() {
    // inst-ad-transport-failed, inst-ad-return-transport, inst-ad-no-content-logs
    let mut causes = Vec::new();
    for failure in [TransportFailure::Gateway, TransportFailure::Transport] {
        let rig = rig(Scripted::Fail(failure));
        let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60)).await;
        let Err(SummaryCallFailure::Other { cause }) = result else {
            panic!("{failure:?}: {result:?}");
        };
        assert!(!cause.is_empty());
        assert!(
            !cause.contains("http") && !cause.contains("example.com"),
            "{cause}"
        );
        causes.push(cause);
    }
    assert_ne!(causes[0], causes[1], "the cause names the failure kind");
}

#[tokio::test]
async fn a_context_length_rejection_is_prompt_too_long() {
    // inst-ad-return-ptl
    let rig = rig(too_long());
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60)).await;
    assert_eq!(result, Err(SummaryCallFailure::PromptTooLong));
}

#[tokio::test]
async fn another_status_is_a_failure_with_the_status_and_no_body_text() {
    // inst-ad-return-other
    for status in [400, 401, 429, 500, 502, 503, 504] {
        let rig = rig(Scripted::status(
            status,
            &json!({ "error": { "message": "PROVIDER-SECRET-TEXT https://x.example/y" } }),
        ));
        let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60)).await;
        let Err(SummaryCallFailure::Other { cause }) = result else {
            panic!("{status}: {result:?}");
        };
        assert!(cause.contains(&status.to_string()), "{status}: {cause}");
        assert!(
            !cause.contains("PROVIDER-SECRET-TEXT") && !cause.contains("https://"),
            "{cause}"
        );
        assert_eq!(
            rig.transport.call_count(),
            1,
            "no retry inside the adapter call"
        );
    }
}

#[tokio::test]
async fn a_success_body_over_the_read_bound_is_malformed() {
    // inst-ad-parse, inst-ad-malformed, inst-ad-return-malformed
    let rig = rig(Scripted::TooLarge);
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60)).await;
    assert!(is_other(&result, MALFORMED), "{result:?}");
}

#[tokio::test]
async fn a_success_body_that_is_not_json_is_malformed() {
    // inst-ad-malformed
    let rig = rig(Scripted::Answer {
        status: 200,
        body: b"<html>ok</html>".to_vec(),
    });
    let result = once(&rig, ProviderKind::OpenaiResponses, Duration::from_secs(60)).await;
    assert!(is_other(&result, MALFORMED), "{result:?}");
}

#[tokio::test]
async fn the_body_is_read_as_the_kind_of_the_provider() {
    // inst-ad-return: the response text and usage are those of the kind
    let anthropic = crate::test_support::maintenance::anthropic_answer("A", (3, 4));
    let rig = rig(Scripted::ok(&anthropic));
    let completion = once(
        &rig,
        ProviderKind::AnthropicMessages,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(completion.text, "A");
    assert_eq!(completion.usage.output_tokens, 4);
}

// ── the provider-call algorithm ──────────────────────────────────────────────

fn settings(claim_secs: u64) -> SummaryWorkerSettings {
    SummaryWorkerSettings {
        claim_timeout: Duration::from_secs(claim_secs),
        ..settings_for(SUMMARY_MODEL)
    }
}

async fn run(
    rig: &Rig,
    settings: &SummaryWorkerSettings,
    base: Option<&str>,
    messages: &[RangeMessage],
) -> Result<SummaryCompletion, SummaryCallFailure> {
    let model = model();
    let provider = provider();
    let prompt: SummaryPrompt = build_prompt(&model, settings, base, messages);
    rig.caller
        .call(&ProviderCall {
            provider: &provider,
            model: &model,
            tenant_id: TENANT_A,
            chat_id: CHAT,
            settings,
            base_summary: base,
            prompt: &prompt,
        })
        .await
}

fn user_of(body: &Value) -> String {
    body["input"][0]["content"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn the_first_attempt_sends_the_fitted_prompt_with_the_system_identity() {
    // inst-call-build, inst-call-try, inst-call-post
    let rig = rig(good((10, 5)));
    let cfg = settings(300);
    let messages = range_of(4);
    let completion = run(&rig, &cfg, None, &messages).await.expect("success");
    let calls = rig.transport.calls();
    assert_eq!(calls.len(), 1);
    let model = model();
    let prompt = build_prompt(&model, &cfg, None, &messages);
    assert_eq!(calls[0].body["instructions"], prompt.system);
    assert_eq!(user_of(&calls[0].body), prompt.user);
    assert_eq!(calls[0].body["metadata"]["request_type"], "summary");
    assert_eq!(
        calls[0].body["max_output_tokens"],
        u64::from(model.max_output_tokens)
    );
    assert_eq!(completion.usage.output_tokens, 5);
}

#[tokio::test]
async fn the_provider_phase_is_bounded_by_the_claim_timeout_minus_ten_seconds() {
    // inst-call-deadline: a hung call ends as a failure before the lease expires
    let rig = rig(Scripted::Hang);
    let result = run(&rig, &settings(30), None, &range_of(4)).await;
    assert!(
        matches!(result, Err(SummaryCallFailure::Other { .. })),
        "{result:?}"
    );
    assert_eq!(
        rig.time.elapsed(),
        Duration::from_secs(20),
        "30 s lease minus 10 s"
    );
    let long = self::rig(Scripted::Hang);
    drop(run(&long, &settings(300), None, &range_of(4)).await);
    assert_eq!(long.time.elapsed(), Duration::from_secs(290));
}

#[tokio::test]
async fn every_attempt_gets_the_time_left_of_the_single_bound() {
    // inst-call-deadline: prompt-too-long retries included; 8 + 8 + 4 = 20 s
    let rig = rig(Scripted::Hang);
    rig.transport
        .queue(Scripted::takes(Duration::from_secs(8), too_long()));
    rig.transport
        .queue(Scripted::takes(Duration::from_secs(8), too_long()));
    let result = run(&rig, &settings(30), None, &range_of(10)).await;
    assert!(
        matches!(result, Err(SummaryCallFailure::Other { .. })),
        "{result:?}"
    );
    assert_eq!(rig.transport.call_count(), 3);
    let starts: Vec<Duration> = rig.transport.calls().iter().map(|call| call.at).collect();
    assert_eq!(
        starts,
        vec![
            Duration::ZERO,
            Duration::from_secs(8),
            Duration::from_secs(16)
        ]
    );
    assert_eq!(
        rig.time.elapsed(),
        Duration::from_secs(20),
        "the third attempt had 4 s left"
    );
}

#[tokio::test]
async fn an_attempt_without_time_left_fails_without_a_call() {
    // inst-call-deadline: the first attempt used the whole bound
    let rig = rig(good((1, 1)));
    rig.transport
        .queue(Scripted::takes(Duration::from_secs(20), too_long()));
    let result = run(&rig, &settings(30), None, &range_of(10)).await;
    assert!(is_other(&result, NO_TIME), "{result:?}");
    assert_eq!(rig.transport.call_count(), 1, "the retry made no call");
}

#[tokio::test]
async fn a_prompt_too_long_rejection_is_retried_with_the_oldest_fifth_dropped() {
    // inst-call-catch-ptl, inst-call-ptl-retry
    let rig = rig(good((10, 5)));
    rig.transport.queue(too_long());
    let cfg = settings(300);
    let messages = range_of(10);
    let completion = run(&rig, &cfg, None, &messages)
        .await
        .expect("the retry succeeds");
    let calls = rig.transport.calls();
    assert_eq!(calls.len(), 2);
    let first_kept = drop_oldest_fifth(&messages).to_vec();
    assert_eq!(first_kept.len(), 8);
    assert_eq!(
        user_of(&calls[1].body),
        render_user_prompt(None, &first_kept, cfg.message_content_limit)
    );
    assert_ne!(user_of(&calls[0].body), user_of(&calls[1].body));
    assert_eq!(calls[1].body["instructions"], calls[0].body["instructions"]);
    assert_eq!(calls[1].uri, calls[0].uri);
    assert_eq!(completion.usage.output_tokens, 5);
}

#[tokio::test]
async fn at_most_two_retries_are_made() {
    // inst-call-ptl-exhausted: three attempts, then the failure
    let rig = rig(too_long());
    let cfg = settings(300);
    let messages = range_of(10);
    let result = run(&rig, &cfg, None, &messages).await;
    assert_eq!(result, Err(SummaryCallFailure::PromptTooLong));
    let calls = rig.transport.calls();
    assert_eq!(calls.len(), 3, "the first call and two retries");
    let once = drop_oldest_fifth(&messages);
    let twice = drop_oldest_fifth(once);
    assert_eq!(
        user_of(&calls[1].body),
        render_user_prompt(None, once, cfg.message_content_limit)
    );
    assert_eq!(
        user_of(&calls[2].body),
        render_user_prompt(None, twice, cfg.message_content_limit)
    );
    assert_eq!(twice.len(), 6, "10 -> 8 -> 6");
}

#[tokio::test]
async fn no_retry_is_made_with_two_or_fewer_messages() {
    // inst-call-ptl-retry: only while more than two messages are kept
    for count in [1, 2] {
        let rig = rig(too_long());
        let result = run(&rig, &settings(300), None, &range_of(count)).await;
        assert_eq!(result, Err(SummaryCallFailure::PromptTooLong), "{count}");
        assert_eq!(rig.transport.call_count(), 1, "{count}");
    }
}

#[tokio::test]
async fn the_retries_stop_at_two_kept_messages() {
    // inst-call-ptl-retry: at least two messages stay; 3 -> 2, then no retry
    let rig = rig(too_long());
    let result = run(&rig, &settings(300), None, &range_of(3)).await;
    assert_eq!(result, Err(SummaryCallFailure::PromptTooLong));
    assert_eq!(
        rig.transport.call_count(),
        2,
        "3 messages, one retry with 2"
    );
    let last = user_of(&rig.transport.calls()[1].body);
    assert!(
        last.contains("Assistant: m2") && last.contains("User: m3") && !last.contains("User: m1")
    );
}

#[tokio::test]
async fn the_retried_prompt_keeps_the_existing_summary() {
    // inst-call-ptl-retry: rebuild the user prompt over the same base
    let rig = rig(good((1, 1)));
    rig.transport.queue(too_long());
    run(&rig, &settings(300), Some("BASE SUMMARY"), &range_of(10))
        .await
        .unwrap();
    for call in rig.transport.calls() {
        assert!(user_of(&call.body).contains("BASE SUMMARY"));
    }
}

#[tokio::test]
async fn rejected_attempts_contribute_no_usage() {
    // inst-call-return: the usage of the successful call only
    let rig = rig(good((111, 22)));
    rig.transport.queue(too_long());
    rig.transport.queue(too_long());
    let completion = run(&rig, &settings(300), None, &range_of(10))
        .await
        .unwrap();
    assert_eq!(completion.usage.input_tokens, 111);
    assert_eq!(completion.usage.output_tokens, 22);
}

#[tokio::test]
async fn any_other_failure_is_returned_without_a_retry() {
    // inst-call-catch-other, inst-call-return-failure: a non-2xx answer, a gateway error, a
    // transport error, a malformed body
    let cases: Vec<Scripted> = vec![
        Scripted::status(500, &json!({})),
        Scripted::status(429, &json!({})),
        Scripted::Fail(TransportFailure::Gateway),
        Scripted::Fail(TransportFailure::Transport),
        Scripted::Answer {
            status: 200,
            body: b"nope".to_vec(),
        },
        Scripted::TooLarge,
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let rig = rig(case);
        let result = run(&rig, &settings(300), None, &range_of(10)).await;
        assert!(
            matches!(result, Err(SummaryCallFailure::Other { .. })),
            "{index}: {result:?}"
        );
        assert_eq!(rig.transport.call_count(), 1, "{index}: no retry");
    }
}

#[tokio::test]
async fn a_failure_after_a_prompt_too_long_rejection_is_not_retried_again() {
    // inst-call-catch-other: only context-length rejections retry
    let rig = rig(good((1, 1)));
    rig.transport.queue(too_long());
    rig.transport.queue(Scripted::status(503, &json!({})));
    let result = run(&rig, &settings(300), None, &range_of(10)).await;
    assert!(
        matches!(result, Err(SummaryCallFailure::Other { .. })),
        "{result:?}"
    );
    assert_eq!(rig.transport.call_count(), 2);
}

#[tokio::test]
async fn the_response_text_and_the_usage_are_returned() {
    // inst-call-return
    let rig = rig(Scripted::ok(&responses_answer(
        &summary_text("final"),
        (9, 3),
    )));
    let completion = run(&rig, &settings(300), None, &range_of(4)).await.unwrap();
    assert_eq!(completion.text, summary_text("final"));
    assert_eq!(completion.usage.input_tokens, 9);
    assert_eq!(completion.usage.output_tokens, 3);
}

#[tokio::test]
async fn the_timer_is_the_injected_one() {
    // the bound is measured on the injected timer, not on the wall clock
    let rig = rig(Scripted::takes(Duration::from_secs(5), good((1, 1))));
    let before = rig.time.monotonic();
    run(&rig, &settings(300), None, &range_of(4)).await.unwrap();
    assert_eq!(rig.time.monotonic(), before + Duration::from_secs(5));
}
