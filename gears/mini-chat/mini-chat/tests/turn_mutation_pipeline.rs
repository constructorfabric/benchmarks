//! A retry / edit goes through the full send pipeline (slice S5): the quota preflight before the
//! mutation commits (a rejection leaves the previous turn in place), kill switches, the downgrade
//! cascade, the reserve before the provider call, settlement and the usage event exactly once,
//! context assembly without the replaced turn, the carry-forward of attachments and the tool
//! settings, and the failures after the mutation commit.
//!
//! Source of truth: DESIGN 3.9 rules 6 and 8 and "Retry Last Turn" / "Edit Last Turn" ("PATCH MUST
//! NOT introduce a separate execution path for settlement or outbox emission"), 3.6 "Retry / edit
//! variant" (steps 1 - 5, the failure of step 4), 3.7 `chat_turns` "Preflight columns on
//! retry/edit", `message_attachments`, 5.7 "Unstarted retry/edit turn" (a setup failure after the
//! commit: plain CAS to `failed`, no reserve, no settlement, no usage event), 5.4 (reserve,
//! settlement), 4 "Downgrade Decision Flow", "Web Search Quota Enforcement".

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
mod mutation_support;
mod quota_support;
mod stream_support;

use mini_chat_sdk::{AuditEvent, BillingOutcome, SettlementMethod, TerminalState};
use mutation_support::{
    AttachmentSeed, LimitsSpec, MutationHarness, Op, Opts, PREMIUM, PREMIUM_IN, PREMIUM_OUT,
    PairSeed, ProviderReply, STANDARD, STREAMING_MAX_OUTPUT, TENANT_A, USER_A, answer_with_usage,
    assert_400_reason, assert_exhausted, assert_field_violation, assert_precondition,
    assert_problem, audit_of, dedupe_key, find_model, merge, quota_models, ref_credits,
    ref_reserve, web_search_pieces,
};
use serde_json::{Value, json};
use uuid::Uuid;

const Q: &str = "hello";

fn premium_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, PREMIUM_IN, PREMIUM_OUT)
}

/// `quota_models()` with `patch` merged into the premium entry.
fn patched_premium(patch: &Value) -> Vec<Value> {
    let mut models = quota_models();
    merge(&mut models[0], patch);
    models
}

// ═════════════════════════════════════════════════════════════════════════════
// the quota preflight runs before the mutation commits (rule 8)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_exhausted_quota_is_429_tokens_and_the_previous_turn_stays() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        h.policy.set_limits(LimitsSpec::new(0, 0, 0, 0));
        let before = h.snapshot().await;

        let resp = h.reject(op, chat, pair.request_id).await;

        assert_exhausted(&resp, "tokens");
        assert_eq!(h.snapshot().await, before, "{op:?}: the old turn is intact");
        assert!(h.turn(pair.request_id).await.deleted_at.is_none());
        h.oagw.assert_no_provider_call();
        h.audit.assert_events_stay(0).await;
    }
}

#[tokio::test]
async fn a_delete_needs_no_quota() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.policy.set_limits(LimitsSpec::new(0, 0, 0, 0));
    assert_eq!(h.delete(chat, pair.request_id).await.status.as_u16(), 204);
}

#[tokio::test]
async fn the_daily_web_search_quota_applies_to_a_mutation_of_a_web_search_turn() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::with_config(json!({"quota": {"web_search_daily_quota": 1}})).await;
        let chat = h.seed_chat().await;
        h.oagw.push(ProviderReply::text_with(
            &["found", " it"],
            &Opts::default()
                .usage(900, 100)
                .insert_after(1, web_search_pieces()),
        ));
        let first = h.say_web(chat, "search for it").await;
        assert_eq!(first.terminal().event, "done");
        assert_eq!(
            h.bucket("total", "daily").await.unwrap().web_search_calls,
            1,
            "the first turn used the daily quota"
        );
        let before = h.snapshot().await;
        let calls = h.oagw.request_count();

        let resp = h.reject(op, chat, first.request_id()).await;

        assert_exhausted(&resp, "web_search");
        assert_eq!(h.snapshot().await, before, "{op:?}");
        assert_eq!(h.oagw.request_count(), calls);
    }
}

#[tokio::test]
async fn the_web_search_kill_switch_rejects_the_mutation_of_a_web_search_turn() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h
            .seed_pair(chat, PairSeed::completed("search it", "A").web_search())
            .await;
        h.policy
            .set_kill_switches(json!({"disable_web_search": true}));
        let before = h.snapshot().await;

        let resp = h.reject(op, chat, pair.request_id).await;

        let problem = assert_problem(&resp, 400, "failed_precondition");
        assert_precondition(&problem, "web_search", "FEATURE_DISABLED");
        assert_eq!(h.snapshot().await, before, "{op:?}: the old turn is intact");
        h.oagw.assert_no_provider_call();
    }
}

#[tokio::test]
async fn the_web_search_kill_switch_does_not_touch_a_mutation_without_web_search() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.policy
        .set_kill_switches(json!({"disable_web_search": true}));

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(sent.terminal().event, "done");
    assert!(h.oagw.last_request().tool("web_search").is_none());
}

#[tokio::test]
async fn the_force_standard_kill_switch_downgrades_the_new_turn() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        h.policy
            .set_kill_switches(json!({"force_standard_tier": true}));

        let sent = match op {
            Op::Retry => h.retried(chat, pair.request_id).await,
            _ => h.edited(chat, pair.request_id, "an edited question").await,
        };

        let done = sent.done();
        assert_eq!(done["effective_model"], json!(STANDARD), "{op:?}");
        assert_eq!(done["selected_model"], json!(PREMIUM));
        assert_eq!(done["quota_decision"], json!("downgrade"));
        assert_eq!(done["downgrade_from"], json!(PREMIUM));
        assert_eq!(done["downgrade_reason"], json!("force_standard_tier"));
        assert_eq!(
            h.oagw.last_request().model(),
            stream_support::provider_model(STANDARD)
        );
        let turn = h.turn(sent.request_id()).await;
        assert_eq!(turn.effective_model.as_deref(), Some(STANDARD));
        assert_eq!(
            h.chat.chat_row(chat).await.unwrap().model,
            PREMIUM,
            "the chat's model never changes"
        );
        assert!(
            h.bucket("tier:premium", "daily").await.is_none(),
            "a standard turn takes no premium reserve"
        );
        let usage = h.policy.wait_published(1).await;
        assert_eq!(usage[0].effective_model, STANDARD);
        assert_eq!(usage[0].selected_model, PREMIUM);
    }
}

#[tokio::test]
async fn an_exhausted_premium_tier_downgrades_a_mutation_instead_of_rejecting_it() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.policy
        .set_limits(LimitsSpec::new(1_000_000_000, 10_000_000_000, 0, 0));

    let sent = h.retried(chat, pair.request_id).await;

    let done = sent.done();
    assert_eq!(done["effective_model"], json!(STANDARD));
    assert_eq!(done["downgrade_reason"], json!("premium_quota_exhausted"));
    let assistant = h
        .messages_in(chat)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(sent.request_id()) && m.role == "assistant")
        .unwrap();
    assert_eq!(assistant.model.as_deref(), Some(STANDARD));
}

#[tokio::test]
async fn the_mutation_turn_is_planned_under_the_current_policy_version() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.policy.publish_version(7, &quota_models());

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(
        h.turn(sent.request_id()).await.policy_version_applied,
        Some(7)
    );
    let usage = h.policy.wait_published(1).await;
    assert_eq!(usage[0].policy_version_applied, 7);
}

// ═════════════════════════════════════════════════════════════════════════════
// image guards on the carried-forward attachments
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_image_of_the_original_message_needs_a_vision_model() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        // `gpt-standard` has no VISION_INPUT capability
        let chat = h.seed_chat_on(STANDARD).await;
        let pair = h
            .seed_pair(
                chat,
                PairSeed::completed("what is in it", "A").model(STANDARD),
            )
            .await;
        let image = h
            .chat
            .seed_attachment(AttachmentSeed::image(chat, "photo.png"))
            .await;
        h.chat.link_attachment(chat, pair.user_message, image).await;
        let before = h.snapshot().await;

        let resp = h.reject(op, chat, pair.request_id).await;

        let problem = assert_400_reason(&resp, "invalid_argument", "VISION_NOT_SUPPORTED");
        assert_field_violation(&problem, "content_type", "VISION_NOT_SUPPORTED");
        assert_eq!(h.snapshot().await, before, "{op:?}: the old turn is intact");
        h.oagw.assert_no_provider_call();
    }
}

#[tokio::test]
async fn an_image_of_the_original_message_is_resubmitted_to_a_vision_model_and_reserved() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let image = h
        .chat
        .seed_attachment(AttachmentSeed::image(chat, "photo.png"))
        .await;
    h.chat.link_attachment(chat, pair.user_message, image).await;

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(sent.terminal().event, "done");
    let new_user = h.user_message_of(chat, sent.request_id()).await;
    assert_eq!(h.links_of(new_user.id).await, vec![image]);
    // the reserve counts the image like the image of a new message (1000 tokens)
    let (_, _, tokens, credits) = ref_reserve(
        &find_model(&quota_models(), PREMIUM),
        STREAMING_MAX_OUTPUT,
        Q,
        0,
        1_000,
    );
    let turn = h.turn(sent.request_id()).await;
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));
}

#[tokio::test]
async fn the_carried_forward_attachments_obey_the_per_message_limits_of_a_send() {
    // limit = max_documents_per_chat + max_images_per_message = 2
    let h = MutationHarness::with_config(
        json!({"rag": {"max_documents_per_chat": 1, "max_images_per_message": 1}}),
    )
    .await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let mut attachments = Vec::new();
    for name in ["a.pdf", "b.pdf"] {
        attachments.push(
            h.chat
                .seed_attachment(AttachmentSeed::document(chat, name))
                .await,
        );
    }
    attachments.push(
        h.chat
            .seed_attachment(AttachmentSeed::image(chat, "c.png"))
            .await,
    );
    for attachment in &attachments {
        h.chat
            .link_attachment(chat, pair.user_message, *attachment)
            .await;
    }
    let before = h.snapshot().await;

    let resp = h.reject(Op::Retry, chat, pair.request_id).await;

    assert_400_reason(&resp, "invalid_argument", "invalid_attachment");
    assert_eq!(h.snapshot().await, before);

    // a deleted attachment is not carried forward, so it does not count against the limit
    let other = h.seed_chat().await;
    let pair = h.seed_pair(other, PairSeed::completed(Q, "A")).await;
    let live_doc = doc(&h, other, "live.pdf").await;
    let live_image = h
        .chat
        .seed_attachment(AttachmentSeed::image(other, "live.png"))
        .await;
    let gone = h
        .chat
        .seed_attachment(AttachmentSeed::document(other, "gone.pdf").deleted())
        .await;
    for attachment in [live_doc, live_image, gone] {
        h.chat
            .link_attachment(other, pair.user_message, attachment)
            .await;
    }
    let sent = h.retried(other, pair.request_id).await;
    assert_eq!(sent.terminal().event, "done");
    let new_user = h.user_message_of(other, sent.request_id()).await;
    assert_eq!(h.links_of(new_user.id).await.len(), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// reserve before the provider call
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_new_turn_holds_its_reserve_and_the_old_one_is_replaced_when_the_provider_is_called() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;

    let sent = h.retried(chat, pair.request_id).await;
    let new = sent.request_id();

    let seen = h.observed.snapshot(0);
    let new_turn = seen
        .turns
        .iter()
        .find(|t| t.request_id == new)
        .expect("the new turn exists when the provider is called");
    assert_eq!(new_turn.state, "running");
    let old_turn = seen
        .turns
        .iter()
        .find(|t| t.request_id == pair.request_id)
        .unwrap();
    assert!(
        old_turn.deleted_at.is_some(),
        "the old turn is replaced by then"
    );
    assert_eq!(old_turn.replaced_by_request_id, Some(new));

    let (est, max_out, tokens, credits) = ref_reserve(
        &find_model(&quota_models(), PREMIUM),
        STREAMING_MAX_OUTPUT,
        Q,
        0,
        0,
    );
    assert_eq!(est, 113);
    assert_eq!(
        new_turn.reserve_tokens,
        Some(i64::try_from(tokens).unwrap())
    );
    assert_eq!(
        new_turn.max_output_tokens_applied,
        Some(i32::try_from(max_out).unwrap())
    );
    assert_eq!(new_turn.reserved_credits_micro, Some(credits));
    assert_eq!(new_turn.policy_version_applied, Some(1));
    assert_eq!(new_turn.effective_model.as_deref(), Some(PREMIUM));
    assert_eq!(new_turn.minimal_generation_floor_applied, Some(50));
    for row in seen
        .usage
        .iter()
        .filter(|r| r.bucket == "total" || r.bucket == "tier:premium")
    {
        assert_eq!(
            row.reserved_credits_micro, credits,
            "{}/{}",
            row.bucket, row.period_type
        );
    }
    assert_eq!(seen.usage.len(), 4, "total + tier:premium, daily + monthly");
}

#[tokio::test]
async fn an_edit_reserves_for_the_edited_content() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let edited = "x".repeat(400);

    let sent = h.edited(chat, pair.request_id, &edited).await;

    let model = find_model(&quota_models(), PREMIUM);
    let (_, _, tokens, credits) = ref_reserve(&model, STREAMING_MAX_OUTPUT, &edited, 0, 0);
    let (_, _, original_tokens, _) = ref_reserve(&model, STREAMING_MAX_OUTPUT, Q, 0, 0);
    assert_ne!(tokens, original_tokens);
    let turn = h.turn(sent.request_id()).await;
    assert_eq!(turn.reserve_tokens, Some(i64::try_from(tokens).unwrap()));
    assert_eq!(turn.reserved_credits_micro, Some(credits));
}

// ═════════════════════════════════════════════════════════════════════════════
// settlement and the usage event: exactly once per new turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_settles_once_and_publishes_one_usage_event() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(answer_with_usage(12, 5));
    let first = h.say(chat, Q).await;
    h.oagw.push(answer_with_usage(100, 40));

    let sent = h.retried(chat, first.request_id()).await;
    let new = sent.request_id();

    let events = h.policy.wait_published(2).await;
    h.policy.assert_published_stays(2).await;
    let mine: Vec<_> = events.iter().filter(|e| e.request_id == new).collect();
    assert_eq!(
        mine.len(),
        1,
        "one usage event for the new turn: {events:?}"
    );
    let event = mine[0];
    let turn = h.turn(new).await;
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.user_id, Some(USER_A));
    assert_eq!(event.turn_id, Some(turn.id));
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, premium_credits(100, 40));
    let usage = event.usage.as_ref().expect("actual usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (100, 40));
    assert_eq!(event.dedupe_key, dedupe_key(TENANT_A, turn.id, new));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.request_id == first.request_id())
            .count(),
        1,
        "the replaced turn's event is not repeated"
    );

    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.unwrap();
        assert_eq!(total.reserved_credits_micro, 0, "{period}");
        assert_eq!(
            total.spent_credits_micro,
            premium_credits(12, 5) + premium_credits(100, 40),
            "{period}: the replaced turn's settlement stays, the new one adds to it"
        );
        assert_eq!(total.calls, 2);
        assert_eq!((total.input_tokens, total.output_tokens), (112, 45));
        let tier = h.bucket("tier:premium", period).await.unwrap();
        assert_eq!((tier.reserved_credits_micro, tier.calls), (0, 2));
    }
}

#[tokio::test]
async fn an_edit_settles_once_like_a_send() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.oagw.push(answer_with_usage(80, 20));

    let sent = h.edited(chat, pair.request_id, "an edited question").await;

    let events = h.policy.wait_published(1).await;
    h.policy.assert_published_stays(1).await;
    assert_eq!(events[0].request_id, sent.request_id());
    assert_eq!(events[0].actual_credits_micro, premium_credits(80, 20));
    h.assert_nothing_reserved().await;
    assert_eq!(h.bucket("total", "daily").await.unwrap().calls, 1);
}

#[tokio::test]
async fn a_failed_retry_settles_estimated_and_the_chat_stays_usable() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.oagw.push(ProviderReply::http_error(500, "boom"));

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(sent.terminal().event, "error");
    assert_eq!(sent.error()["code"], json!("provider_error"));
    let turn = h.turn(sent.request_id()).await;
    assert_eq!(turn.state, "failed");
    assert!(
        h.turn(pair.request_id).await.deleted_at.is_some(),
        "still replaced"
    );
    let events = h.policy.wait_published(1).await;
    assert_eq!(events[0].terminal_state, TerminalState::Failed);
    assert_eq!(events[0].settlement_method, SettlementMethod::Estimated);
    h.assert_nothing_reserved().await;

    let next = h.say(chat, "and now").await;
    assert_eq!(next.terminal().event, "done");
}

#[tokio::test]
async fn a_cancelled_retry_keeps_its_partial_answer_and_settles_estimated() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.oagw.push(ProviderReply::text_with(
        &["Hel", "lo", " world"],
        &Opts::default().pause_after(2),
    ));
    let mut live = h.retry(chat, pair.request_id).await.live();
    let started = live.expect("stream_started").await;
    let new = uuid::Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap();
    live.next_non_ping().await.unwrap();
    live.next_non_ping().await.unwrap();
    h.oagw.wait_paused(1).await;

    drop(live);

    let turn = h.wait_turn_state(new, "cancelled").await;
    assert!(turn.assistant_message_id.is_some());
    let live_texts = h.live_texts(chat).await;
    assert_eq!(
        live_texts.last().unwrap(),
        &("assistant".to_owned(), "Hello".to_owned())
    );
    let events = h.policy.wait_published(1).await;
    assert_eq!(events[0].terminal_state, TerminalState::Cancelled);
    assert_eq!(events[0].billing_outcome, BillingOutcome::Aborted);
    assert_eq!(events[0].settlement_method, SettlementMethod::Estimated);
    h.assert_nothing_reserved().await;
}

// ═════════════════════════════════════════════════════════════════════════════
// tools follow the original turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_of_a_web_search_turn_sends_the_tool_and_counts_its_own_calls() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h
        .seed_pair(chat, PairSeed::completed("search for it", "A").web_search())
        .await;
    h.oagw.push(ProviderReply::text_with(
        &["found", " it"],
        &Opts::default()
            .usage(900, 100)
            .insert_after(1, web_search_pieces()),
    ));

    let sent = h.retried(chat, pair.request_id).await;

    assert!(
        sent.names().contains(&"tool".to_owned()),
        "{:?}",
        sent.names()
    );
    assert!(
        h.oagw.last_request().tool("web_search").is_some(),
        "the retried turn keeps web search"
    );
    let turn = h.turn(sent.request_id()).await;
    assert!(turn.web_search_enabled);
    assert_eq!(turn.web_search_completed_count, 1);
    assert_eq!(h.turn(pair.request_id).await.web_search_completed_count, 0);
    let events = h.policy.wait_published(1).await;
    assert_eq!(events[0].web_search_calls, 1);
    assert_eq!(
        h.bucket("total", "daily").await.unwrap().web_search_calls,
        1
    );
}

#[tokio::test]
async fn an_edit_keeps_the_web_search_setting_and_a_plain_turn_stays_plain() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let web = h
        .seed_pair(
            chat,
            PairSeed::completed("search it", "A").web_search().at(10),
        )
        .await;
    let sent = h
        .edited(chat, web.request_id, "search something else")
        .await;
    assert!(h.oagw.last_request().tool("web_search").is_some());
    assert!(h.turn(sent.request_id()).await.web_search_enabled);

    let plain_chat = h.seed_chat().await;
    let plain = h.seed_pair(plain_chat, PairSeed::completed(Q, "A")).await;
    let sent = h.retried(plain_chat, plain.request_id).await;
    assert!(h.oagw.last_request().tool("web_search").is_none());
    assert!(!h.turn(sent.request_id()).await.web_search_enabled);
}

#[tokio::test]
async fn file_search_is_available_to_a_mutation_like_to_a_send() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.seed_document_with_store(chat).await;

    h.retried(chat, pair.request_id).await;

    let request = h.oagw.last_request();
    assert!(
        request.tool_types().contains(&"file_search".to_owned()),
        "{:?}",
        request.tool_types()
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// context assembly excludes the replaced turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_sends_the_history_before_the_replaced_turn_and_the_original_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;

    h.retried(chat, pairs[2].request_id).await;

    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:question 1".to_owned(),
            "assistant:answer 1".to_owned(),
            "user:question 2".to_owned(),
            "assistant:answer 2".to_owned(),
            "user:question 3".to_owned()
        ]
    );
}

#[tokio::test]
async fn an_edit_sends_the_history_before_the_replaced_turn_and_the_edited_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 3).await;

    h.edited(chat, pairs[2].request_id, "question 3, reworded")
        .await;

    let conversation = h.oagw.last_request().conversation_texts();
    assert_eq!(
        conversation,
        vec![
            "user:question 1".to_owned(),
            "assistant:answer 1".to_owned(),
            "user:question 2".to_owned(),
            "assistant:answer 2".to_owned(),
            "user:question 3, reworded".to_owned()
        ]
    );
    assert!(
        conversation.iter().all(|m| !m.contains("answer 3")),
        "the replaced answer is not in the context"
    );
}

#[tokio::test]
async fn the_next_message_after_an_edit_sees_the_edited_turn_and_not_the_original() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 1).await;
    h.oagw.push(ProviderReply::text(&["the new answer"]));
    h.edited(chat, pairs[0].request_id, "question 1, reworded")
        .await;

    h.say(chat, "a follow-up").await;

    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:question 1, reworded".to_owned(),
            "assistant:the new answer".to_owned(),
            "user:a follow-up".to_owned()
        ]
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// attachments are carried forward
// ═════════════════════════════════════════════════════════════════════════════

async fn doc(h: &MutationHarness, chat: Uuid, name: &str) -> Uuid {
    h.chat
        .seed_attachment(AttachmentSeed::document(chat, name))
        .await
}

#[tokio::test]
async fn a_retry_copies_the_attachment_links_to_the_new_user_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let a = doc(&h, chat, "a.pdf").await;
    let b = doc(&h, chat, "b.pdf").await;
    for attachment in [a, b] {
        h.chat
            .link_attachment(chat, pair.user_message, attachment)
            .await;
    }
    let mut expected = vec![a, b];
    expected.sort();

    let sent = h.retried(chat, pair.request_id).await;

    let new_user = h.user_message_of(chat, sent.request_id()).await;
    assert_eq!(
        h.links_of(new_user.id).await,
        expected,
        "copied to the new message"
    );
    assert_eq!(
        h.links_of(pair.user_message).await,
        expected,
        "the old rows stay on the soft-deleted message"
    );
    assert_eq!(h.all_links().await.len(), 4);

    let items = h.listed_messages(chat).await;
    let listed = items
        .iter()
        .find(|m| m["id"] == json!(new_user.id.to_string()))
        .expect("the new user message is listed");
    let mut listed_ids: Vec<String> = listed["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["attachment_id"].as_str().unwrap().to_owned())
        .collect();
    listed_ids.sort();
    assert_eq!(listed_ids, chat_support::ids_of(&expected));
}

#[tokio::test]
async fn an_edit_re_links_the_original_attachments_to_the_edited_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let a = doc(&h, chat, "a.pdf").await;
    h.chat.link_attachment(chat, pair.user_message, a).await;

    let sent = h.edited(chat, pair.request_id, "an edited question").await;

    let new_user = h.user_message_of(chat, sent.request_id()).await;
    assert_eq!(new_user.content, "an edited question");
    assert_eq!(h.links_of(new_user.id).await, vec![a]);
    assert_eq!(h.links_of(pair.user_message).await, vec![a]);
}

#[tokio::test]
async fn deleted_attachments_are_silently_excluded_from_the_copy() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let kept = doc(&h, chat, "kept.pdf").await;
        let gone = h
            .chat
            .seed_attachment(AttachmentSeed::document(chat, "gone.pdf").deleted())
            .await;
        for attachment in [kept, gone] {
            h.chat
                .link_attachment(chat, pair.user_message, attachment)
                .await;
        }

        let sent = match op {
            Op::Retry => h.retried(chat, pair.request_id).await,
            _ => h.edited(chat, pair.request_id, "reworded").await,
        };

        assert_eq!(
            sent.terminal().event,
            "done",
            "{op:?}: no error for the deleted one"
        );
        let new_user = h.user_message_of(chat, sent.request_id()).await;
        assert_eq!(h.links_of(new_user.id).await, vec![kept], "{op:?}");
        assert_eq!(
            h.links_of(pair.user_message).await.len(),
            2,
            "the old rows are untouched"
        );
    }
}

#[tokio::test]
async fn only_the_attachments_of_the_mutated_message_are_carried_forward() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;
    let earlier = doc(&h, chat, "earlier.pdf").await;
    h.chat
        .link_attachment(chat, pairs[0].user_message, earlier)
        .await;

    let sent = h.retried(chat, pairs[1].request_id).await;

    let new_user = h.user_message_of(chat, sent.request_id()).await;
    assert!(h.links_of(new_user.id).await.is_empty());
    assert_eq!(h.all_links().await.len(), 1);
}

// ═════════════════════════════════════════════════════════════════════════════
// failures after the mutation commit (rule 8)
// ═════════════════════════════════════════════════════════════════════════════

/// The turn that replaced `old` (the mutation committed it before the failure).
async fn replacement_of(
    h: &MutationHarness,
    old: Uuid,
) -> mini_chat::infra::db::entity::chat_turns::Model {
    let old_turn = h.turn(old).await;
    assert!(old_turn.deleted_at.is_some(), "the mutation committed");
    let new = old_turn
        .replaced_by_request_id
        .expect("replaced_by_request_id is set");
    h.turn(new).await
}

#[tokio::test]
async fn a_context_that_does_not_fit_fails_the_new_turn_after_the_commit() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        // the window shrinks below the output cap: the quota gate still passes, context
        // assembly (step 4, after the commit) cannot fit anything
        h.policy.set_catalog(&patched_premium(
            &json!({"context_window": 1000, "max_output_tokens": 4096, "max_input_tokens": 0}),
        ));

        let resp = h.reject(op, chat, pair.request_id).await;

        assert_400_reason(&resp, "out_of_range", "CONTEXT_BUDGET_EXCEEDED");
        let new_turn = replacement_of(&h, pair.request_id).await;
        assert_eq!(new_turn.state, "failed", "{op:?}");
        assert_eq!(
            new_turn.error_code.as_deref(),
            Some("context_length_exceeded")
        );
        assert!(
            new_turn.deleted_at.is_none(),
            "the failed turn is the latest one"
        );
        assert!(new_turn.completed_at.is_some());
        h.oagw.assert_no_provider_call();
        assert!(
            h.usage_rows()
                .await
                .iter()
                .all(|r| r.reserved_credits_micro == 0),
            "no reserve exists at that point"
        );
        h.policy.assert_published_stays(0).await;

        // the chat is not blocked by a running turn
        h.policy.set_catalog(&quota_models());
        let next = h.say(chat, "carry on").await;
        assert_eq!(next.terminal().event, "done", "{op:?}");
    }
}

#[tokio::test]
async fn a_provider_that_cannot_be_resolved_fails_the_new_turn_with_turn_setup_failed() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        h.policy.set_catalog(&patched_premium(
            &json!({"provider_id": "unconfigured-provider-x"}),
        ));

        let resp = h.reject(op, chat, pair.request_id).await;

        assert_problem(&resp, 500, "internal");
        assert!(
            !resp.raw.contains("unconfigured-provider-x"),
            "the wire error is generic: {}",
            resp.raw
        );
        let new_turn = replacement_of(&h, pair.request_id).await;
        assert_eq!(new_turn.state, "failed", "{op:?}");
        assert_eq!(new_turn.error_code.as_deref(), Some("turn_setup_failed"));
        h.oagw.assert_no_provider_call();
        h.assert_nothing_reserved().await;
        h.policy.assert_published_stays(0).await;

        h.policy.set_catalog(&quota_models());
        let next = h.say(chat, "carry on").await;
        assert_eq!(next.terminal().event, "done", "{op:?}");
    }
}

#[tokio::test]
async fn a_setup_failure_after_the_commit_emits_only_the_mutation_audit_event() {
    for (op, event_type) in [(Op::Retry, "turn_retry"), (Op::Edit, "turn_edit")] {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        h.policy.set_catalog(&patched_premium(
            &json!({"context_window": 1000, "max_output_tokens": 4096, "max_input_tokens": 0}),
        ));
        h.reject(op, chat, pair.request_id).await;

        let events = h.audit.wait_events(1).await;
        h.audit.assert_events_stay(1).await;
        assert_eq!(events[0].event_type(), event_type, "{op:?}");
        assert!(
            audit_of(&events, "turn_failed").is_empty(),
            "no turn_failed event"
        );
        assert!(matches!(
            events[0],
            AuditEvent::TurnRetry { .. } | AuditEvent::TurnEdit { .. }
        ));
    }
}
