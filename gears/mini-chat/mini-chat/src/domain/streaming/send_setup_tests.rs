//! The send setup over in-memory `SQLite`, the real quota service, the real stores and a
//! recording provider-task port: the plan handed to the provider task, the replay, the turn
//! admission order, every preflight rejection (no turn, no reserve, no user message, no provider
//! task), the reserve rows and the pre-provider cancellation check.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: flow `send-message`
//! (setup part) and `resend-request-id`; algorithms `turn-admission-checks`, `replay`,
//! `send-preflight`, `image-guards`, `context-budget`, `context-assembly`, `tool-assembly`,
//! `reserve-transaction`, `attachment-validation`; state `turn-lifecycle` (creation); definitions
//! of done `send-preflight`, `reserve-transaction`, `context-assembly`, `idempotency`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::QuotaDecision;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::provider_request::build_provider_request;
use super::{ProviderCallPlan, SendOutcome, StreamEvent, StreamSettings, ToolKind, ValidatedSend};
use crate::config::MiniChatConfig;
use crate::domain::error::{ConflictReason, DomainError, QuotaScope, ResourceKind};
use crate::domain::quota::PeriodType;
use crate::domain::streaming::request::RawSendBody;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, a2, at, b1};
use crate::test_support::quota::{MetricCall, PREMIUM, STANDARD, default_now, row, seed_rows};
use crate::test_support::send::{
    AttachmentSeed, FILE_GUARD, MessageSeed, ProviderMode, SYSTEM_PROMPT, SendRig, SendRigOptions,
    TurnSeedFull, WEB_GUARD, byte_budgets, catalog_of, default_catalog, footprint, link_rows,
    multi_providers, prem_model, seed_att, seed_completed_turn, seed_msg, seed_summary,
    seed_turn_full, send_rig, send_rig_with, std_model, t, turn_rows, with_web_search,
};
use crate::test_support::{TENANT_A, USER_A1};

const DAY: &str = "2026-02-28";

// ── helpers ──────────────────────────────────────────────────────────────────

fn plain(content: &str) -> ValidatedSend {
    ValidatedSend {
        content: content.to_owned(),
        request_id: None,
        attachment_ids: Vec::new(),
        web_search_enabled: false,
    }
}

fn with_request(mut send: ValidatedSend, request_id: Uuid) -> ValidatedSend {
    send.request_id = Some(request_id);
    send
}

fn with_attachments(mut send: ValidatedSend, ids: &[Uuid]) -> ValidatedSend {
    send.attachment_ids = ids.to_vec();
    send
}

fn with_web(mut send: ValidatedSend) -> ValidatedSend {
    send.web_search_enabled = true;
    send
}

async fn send_as(
    rig: &SendRig,
    who: &toolkit_security::SecurityContext,
    chat: &ChatRecord,
    send: ValidatedSend,
) -> Result<SendOutcome, DomainError> {
    rig.service
        .send(who, chat.id, send, &CancellationToken::new())
        .await
}

async fn send(
    rig: &SendRig,
    chat: &ChatRecord,
    send: ValidatedSend,
) -> Result<SendOutcome, DomainError> {
    send_as(rig, &a1(), chat, send).await
}

fn plan_of(outcome: Result<SendOutcome, DomainError>) -> ProviderCallPlan {
    match outcome {
        Ok(SendOutcome::Streaming(started)) => started.plan,
        other => panic!("expected a started turn, got {other:?}"),
    }
}

fn error_of(outcome: Result<SendOutcome, DomainError>) -> DomainError {
    match outcome {
        Err(error) => error,
        Ok(outcome) => panic!("expected a rejection, got {outcome:?}"),
    }
}

/// Assert a rejection left nothing behind: the footprint of the chat is the one `before` (no
/// user message, no turn, no link, no outbox message, no reserve), and the provider task was not
/// started.
async fn assert_untouched(
    rig: &SendRig,
    chat: &ChatRecord,
    before: &crate::test_support::send::Footprint,
) {
    assert_eq!(
        &footprint(&rig.quota.db, chat).await,
        before,
        "rows were left behind"
    );
    assert_eq!(rig.provider.call_count(), 0, "a provider task was started");
}

fn reserve_calls(rig: &SendRig) -> Vec<MetricCall> {
    rig.quota
        .metrics
        .calls()
        .into_iter()
        .filter(|call| matches!(call, MetricCall::Reserve(_)))
        .collect()
}

fn preflight_calls(rig: &SendRig) -> usize {
    rig.quota
        .metrics
        .calls()
        .into_iter()
        .filter(|call| matches!(call, MetricCall::Preflight { .. }))
        .count()
}

/// A rig whose catalog is the default one with the standard model patched.
async fn rig_with_std(patch: &serde_json::Value) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_snapshot(catalog_of(vec![prem_model(&json!({})), std_model(patch)])),
    )
    .await
}

// ── the plan handed to the provider task ─────────────────────────────────────

#[tokio::test]
async fn a_new_turn_hands_the_assembled_plan_to_the_provider_task() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let request_id = Uuid::new_v4();
    let outcome = send(&rig, &chat, with_request(plain("Hello"), request_id)).await;
    let plan = plan_of(outcome);

    assert_eq!(
        rig.provider.only_plan(),
        plan,
        "the port received the returned plan"
    );
    assert_eq!(plan.tenant_id, TENANT_A);
    assert_eq!(plan.user_id, USER_A1);
    assert_eq!(plan.chat_id, chat.id);
    assert_eq!(plan.request_id, request_id);
    assert_eq!(plan.selected_model, STANDARD);
    assert_eq!(plan.model.id, STANDARD);
    assert_eq!(plan.decision.effective_model, STANDARD);
    assert_eq!(plan.decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(plan.started_at, default_now());
    assert_eq!(plan.decision.started_at, default_now());
    assert_eq!(
        plan.receipt.reserved_credits_micro,
        plan.decision.reserved_credits_micro
    );
    assert!(!plan.web_search_enabled);
    assert!(plan.tools.tools.is_empty());
    assert_eq!(plan.tools.feature, "none");

    assert!(
        plan.context
            .plan
            .system_instructions
            .starts_with(SYSTEM_PROMPT)
    );
    assert_eq!(plan.context.plan.user.text, "Hello");
    assert!(
        plan.context.plan.recent.is_empty(),
        "first message of an empty chat: no history"
    );
    assert!(plan.context.plan.thread_summary.is_none());
    assert_eq!(plan.context.summary_token_estimate, None);
    assert!(!plan.context.messages_truncated);

    // the provider resolved for the effective model and the caller's tenant
    assert_eq!(plan.provider.provider_id, "azure_openai");
    assert_eq!(plan.provider.upstream_alias, "azure.example.com");
}

#[tokio::test]
async fn the_plan_names_the_rows_of_the_committed_reserve_transaction() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, plan.turn_id);
    assert_eq!(turns[0].request_id, plan.request_id);
    assert_eq!(turns[0].state, "running");
    let messages = crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(messages.len(), 1, "only the user message exists");
    assert_eq!(messages[0].id, plan.user_message_id);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "Hello");
    assert_eq!(messages[0].request_id, Some(plan.request_id));
    assert_eq!(
        messages[0].created_at,
        default_now(),
        "created_at is the application clock"
    );
}

#[tokio::test]
async fn the_assistant_message_id_is_a_fresh_uuid_v4_without_a_row() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    assert_eq!(plan.assistant_message_id.get_version_num(), 4);
    assert_ne!(plan.assistant_message_id, plan.user_message_id);
    let rows = crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await;
    assert!(rows.iter().all(|m| m.id != plan.assistant_message_id));
    assert!(rows.iter().all(|m| m.role != "assistant"));
}

#[tokio::test]
async fn a_missing_request_id_is_generated_and_a_supplied_one_of_any_version_is_kept() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let generated = plan_of(send(&rig, &chat, plain("one")).await);
    assert_eq!(generated.request_id.get_version_num(), 4);

    // a terminal turn leaves room for the next one
    for supplied in [
        Uuid::from_u128(0x0123_4567_89ab_1def_8123_4567_89ab_cdef),
        Uuid::from_u128(0x0123_4567_89ab_7def_8123_4567_89ab_cdee),
    ] {
        finish_running_turns(&rig, &chat).await;
        let plan = plan_of(send(&rig, &chat, with_request(plain("again"), supplied)).await);
        assert_eq!(plan.request_id, supplied);
        let turns = turn_rows(&rig.quota.db, chat.id).await;
        assert!(turns.iter().any(|turn| turn.request_id == supplied));
    }
}

/// Move every running turn of the chat to `completed` (what the S2 finalization does), so the
/// next send is not blocked by the parallel-turn guard.
async fn finish_running_turns(rig: &SendRig, chat: &ChatRecord) {
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        &format!(
            "UPDATE chat_turns SET state = 'completed', completed_at = updated_at \
             WHERE chat_id = {} AND state = 'running'",
            crate::test_support::chats::blob(chat.id)
        ),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_provider_task_starts_only_after_the_reserve_transaction_committed() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    let seen = rig.provider.observations();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].running_turns, 1,
        "the running turn exists before the first outbound byte"
    );
    assert_eq!(seen[0].user_messages, 1);
    assert_eq!(
        seen[0].reserved_credits,
        plan.decision.reserved_credits_micro * 2
    );
}

#[tokio::test]
async fn the_reserve_metrics_are_recorded_once_per_period_after_the_commit() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    plan_of(send(&rig, &chat, plain("Hello")).await);
    assert_eq!(
        reserve_calls(&rig),
        vec![
            MetricCall::Reserve(PeriodType::Daily),
            MetricCall::Reserve(PeriodType::Monthly)
        ]
    );
    assert_eq!(preflight_calls(&rig), 1);
}

#[tokio::test]
async fn the_quota_preflight_runs_once_and_its_decision_is_the_booked_reserve() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    assert_eq!(preflight_calls(&rig), 1);
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.reserve_tokens, Some(plan.decision.reserve_tokens));
    assert_eq!(
        turn.reserved_credits_micro,
        Some(plan.decision.reserved_credits_micro)
    );
    assert_eq!(
        turn.effective_model.as_deref(),
        Some(plan.decision.effective_model.as_str())
    );
    assert_eq!(turn.started_at, plan.decision.started_at);
}

// ── downgrade and the effective model ────────────────────────────────────────

#[tokio::test]
async fn a_downgrade_uses_the_effective_model_for_the_plan_and_keeps_the_selected_one() {
    let rig = send_rig().await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    assert_eq!(plan.selected_model, PREMIUM);
    assert_eq!(plan.decision.effective_model, STANDARD);
    assert_eq!(plan.model.id, STANDARD);
    assert_eq!(plan.decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(plan.decision.downgrade_from.as_deref(), Some(PREMIUM));
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.effective_model.as_deref(), Some(STANDARD));
}

#[tokio::test]
async fn a_chat_model_that_is_disabled_but_still_in_the_catalog_is_resolved_without_the_enabled_filter()
 {
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({ "enabled": false })),
        std_model(&json!({})),
    ])))
    .await;
    let chat = rig.prem_chat().await;
    let plan = plan_of(send(&rig, &chat, plain("Hello")).await);
    assert_eq!(plan.selected_model, PREMIUM);
    assert_eq!(plan.decision.effective_model, STANDARD);
    assert_eq!(plan.decision.quota_decision, QuotaDecision::Downgrade);
}

// ── web search ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_search_on_a_supporting_model_adds_the_tool_and_its_guard() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, with_web(plain("search please"))).await);
    assert!(plan.web_search_enabled);
    let kinds: Vec<ToolKind> = plan.tools.tools.iter().map(|tool| tool.kind).collect();
    assert_eq!(kinds, vec![ToolKind::WebSearch]);
    assert_eq!(plan.tools.feature, "web_search");
    assert!(plan.context.plan.system_instructions.ends_with(WEB_GUARD));
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert!(turn.web_search_enabled);
}

#[tokio::test]
async fn web_search_on_a_model_without_support_proceeds_without_the_tool_but_stores_the_flag() {
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({})),
        with_web_search(&std_model(&json!({})), false),
    ])))
    .await;
    let chat = rig.std_chat().await;
    let plan = plan_of(send(&rig, &chat, with_web(plain("search please"))).await);
    assert!(plan.tools.tools.is_empty());
    assert!(!plan.context.plan.system_instructions.contains(WEB_GUARD));
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert!(
        turn.web_search_enabled,
        "the requested flag is stored whether or not the tool is sent"
    );
}

#[tokio::test]
async fn the_kill_switch_rejects_a_web_search_request_before_any_write() {
    let mut snapshot = default_catalog();
    snapshot.kill_switches.disable_web_search = true;
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(snapshot)).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_web(plain("search"))).await);
    assert!(
        matches!(
            error,
            DomainError::FeatureDisabled {
                subject: "web_search"
            }
        ),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
    // a request that does not ask for web search is not affected
    assert!(matches!(
        send(&rig, &chat, plain("plain")).await,
        Ok(SendOutcome::Streaming(_))
    ));
}

#[tokio::test]
async fn an_exhausted_daily_web_search_quota_is_a_web_search_rejection() {
    let rig = send_rig().await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").web(75)]).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_web(plain("search"))).await);
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch
            }
        ),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
}

// ── the context ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_recent_history_is_chronological_within_the_snapshot_and_excludes_the_new_message() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let other = rig.quota.chat_a1(STANDARD).await;
    seed_completed_turn(&rig.quota.db, &chat, "q1", "a1", t(10)).await;
    seed_completed_turn(&rig.quota.db, &chat, "q2", "a2", t(20)).await;
    seed_completed_turn(&rig.quota.db, &other, "other-q", "other-a", t(15)).await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "deleted", t(25)).deleted(t(30)),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "compressed", t(26)).compressed(),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "legacy", t(27)).request(None),
    )
    .await;

    let plan = plan_of(send(&rig, &chat, plain("q3")).await);
    let recent: Vec<&str> = plan
        .context
        .plan
        .recent
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(recent, vec!["q1", "a1", "q2", "a2"]);
    assert_eq!(plan.context.plan.user.text, "q3");
    assert!(!plan.context.messages_truncated);
}

#[tokio::test]
async fn at_most_the_configured_number_of_recent_messages_enter_the_context() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for i in 0..7 {
        seed_completed_turn(
            &rig.quota.db,
            &chat,
            &format!("q{i}"),
            &format!("a{i}"),
            t(i * 10),
        )
        .await;
    }
    let plan = plan_of(send(&rig, &chat, plain("next")).await);
    let recent: Vec<&str> = plan
        .context
        .plan
        .recent
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(recent.len(), 10, "recent_messages_limit is 10");
    assert_eq!(recent[0], "q2");
    assert_eq!(recent[9], "a6");
}

#[tokio::test]
async fn the_thread_summary_enters_the_plan_and_its_estimate_is_reported() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, assistant) =
        seed_completed_turn(&rig.quota.db, &chat, "covered-q", "covered-a", t(10)).await;
    seed_completed_turn(&rig.quota.db, &chat, "fresh-q", "fresh-a", t(20)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "the story so far",
        33,
        (t(10) + time::Duration::microseconds(1), assistant),
    )
    .await;

    let plan = plan_of(send(&rig, &chat, plain("next")).await);
    let item = plan
        .context
        .plan
        .thread_summary
        .as_ref()
        .expect("summary item");
    assert!(item.text.contains("the story so far"));
    assert_eq!(plan.context.summary_token_estimate, Some(33));
    let recent: Vec<&str> = plan
        .context
        .plan
        .recent
        .iter()
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(
        recent,
        vec!["fresh-q", "fresh-a"],
        "messages covered by the summary are not repeated"
    );
}

#[tokio::test]
async fn history_is_truncated_to_the_budget_by_whole_turns() {
    // window 1300, output cap 500: an input limit of 800 bytes at one byte per token
    let rig = rig_with_std(&json!({
        "context_window": 1_300, "max_input_tokens": 0,
        "estimation_budgets": byte_budgets(),
    }))
    .await;
    let chat = rig.std_chat().await;
    let hundred = "x".repeat(100);
    for i in 0..6 {
        seed_completed_turn(&rig.quota.db, &chat, &hundred, &hundred, t(i * 10)).await;
    }
    let plan = plan_of(send(&rig, &chat, plain("the question")).await);
    let context = &plan.context;
    assert!(context.messages_truncated);
    assert!(
        context.assembled_context_tokens <= context.effective_budget,
        "{context:?}"
    );
    let recent = &context.plan.recent;
    assert!(!recent.is_empty() && recent.len() < 12);
    assert_eq!(recent.len() % 2, 0, "whole turns only");
    assert_eq!(
        recent[0].role,
        super::ContextRole::User,
        "never an answer without its question"
    );
}

#[tokio::test]
async fn the_same_chat_state_always_yields_the_same_context() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for i in 0..3 {
        seed_completed_turn(
            &rig.quota.db,
            &chat,
            &format!("q{i}"),
            &format!("a{i}"),
            t(i * 10),
        )
        .await;
    }
    let first = plan_of(send(&rig, &chat, plain("again")).await);
    finish_running_turns(&rig, &chat).await;
    // remove the first turn's user message so the chat state matches the first request again
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        &format!(
            "DELETE FROM messages WHERE id = {}",
            crate::test_support::chats::blob(first.user_message_id)
        ),
    )
    .await
    .unwrap();
    let second = plan_of(send(&rig, &chat, plain("again")).await);
    assert_eq!(first.context, second.context);
    assert_eq!(first.tools, second.tools);
}

// ── images and attachments ───────────────────────────────────────────────────

#[tokio::test]
async fn images_enter_the_plan_in_attachment_order_and_every_attachment_is_linked() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let first = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let document = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    let second = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let plan = plan_of(
        send(
            &rig,
            &chat,
            with_attachments(plain("look"), &[second, document, first]),
        )
        .await,
    );
    let images = &plan.context.plan.user.images;
    assert_eq!(
        images.iter().map(|i| i.attachment_id).collect::<Vec<_>>(),
        vec![second, first]
    );
    for image in images {
        assert!(image.provider_file_id.starts_with("file-"), "{image:?}");
    }
    let linked = link_rows(&rig.quota.db, chat.id).await;
    assert_eq!(linked.len(), 3);
    assert!(linked.iter().all(|l| l.message_id == plan.user_message_id));
}

/// A rig whose premium model (vision input) is served by an `anthropic_messages` provider.
async fn anthropic_vision_rig() -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_snapshot(catalog_of(vec![
                prem_model(&json!({ "provider_id": "anthropic" })),
                std_model(&json!({})),
            ]))
            .with_providers(multi_providers()),
    )
    .await
}

#[tokio::test]
async fn an_anthropic_turn_carries_the_secondary_file_id_of_an_image() {
    let rig = anthropic_vision_rig().await;
    let chat = rig.prem_chat().await;
    let image = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").secondary("file_011CNha8iCJcU1wXNR6q4V8w", "uploaded"),
    )
    .await;
    let plan = plan_of(send(&rig, &chat, with_attachments(plain("look"), &[image])).await);
    let images = &plan.context.plan.user.images;
    assert_eq!(images.len(), 1);
    assert!(
        images[0].provider_file_id.starts_with("file-"),
        "{images:?}"
    );
    assert_eq!(
        images[0].secondary_file_id.as_deref(),
        Some("file_011CNha8iCJcU1wXNR6q4V8w")
    );
    let body = build_provider_request(&plan).unwrap().body;
    assert_eq!(
        body["messages"][0]["content"][1]["source"]["file_id"],
        "file_011CNha8iCJcU1wXNR6q4V8w"
    );
}

#[tokio::test]
async fn an_anthropic_turn_rejects_an_image_without_a_usable_secondary_copy_before_any_provider_call()
 {
    let rig = anthropic_vision_rig().await;
    let chat = rig.prem_chat().await;
    let none = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let pending = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").secondary("file_011pending", "pending"),
    )
    .await;
    let failed = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").secondary("file_011failed", "failed"),
    )
    .await;
    let good = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").secondary("file_011good", "uploaded"),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    for bad in [none, pending, failed] {
        let error = error_of(send(&rig, &chat, with_attachments(plain("see"), &[good, bad])).await);
        assert!(matches!(error, DomainError::InvalidAttachment), "{error:?}");
        assert_untouched(&rig, &chat, &before).await;
    }
}

#[tokio::test]
async fn an_openai_turn_keeps_the_primary_file_id_and_ignores_the_secondary_copy() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let with_copy = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").secondary("file_011CNha8iCJcU1wXNR6q4V8w", "uploaded"),
    )
    .await;
    let without_copy = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let plan = plan_of(
        send(
            &rig,
            &chat,
            with_attachments(plain("look"), &[with_copy, without_copy]),
        )
        .await,
    );
    let body = build_provider_request(&plan).unwrap().body;
    let content = body["input"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3, "{body}");
    for (item, image) in content[1..].iter().zip(&plan.context.plan.user.images) {
        assert_eq!(item["file_id"], image.provider_file_id.as_str());
        assert!(image.provider_file_id.starts_with("file-"), "{image:?}");
    }
}

#[tokio::test]
async fn documents_do_not_count_toward_the_image_limit() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await);
    }
    for _ in 0..4 {
        ids.push(seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await);
    }
    let plan = plan_of(send(&rig, &chat, with_attachments(plain("many"), &ids)).await);
    assert_eq!(plan.context.plan.user.images.len(), 4);
}

// ── replay ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_request_id_is_replayed_without_any_side_effect() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, assistant) =
        seed_completed_turn(&rig.quota.db, &chat, "question", "the stored answer", t(10)).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let gateway_calls = rig.gateway.total_calls();

    let outcome = send(&rig, &chat, with_request(plain("question"), request_id))
        .await
        .unwrap();
    let SendOutcome::Replay(events) = outcome else {
        panic!("expected a replay, got {outcome:?}");
    };
    assert_eq!(events.len(), 3);
    let StreamEvent::StreamStarted(started) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        (started.request_id, started.message_id, started.is_new_turn),
        (request_id, assistant, false)
    );
    let StreamEvent::Delta(delta) = &events[1] else {
        panic!("{events:?}")
    };
    assert_eq!(delta.content, "the stored answer");
    let StreamEvent::Done(done) = &events[2] else {
        panic!("{events:?}")
    };
    assert_eq!((done.input_tokens, done.output_tokens), (120, 30));
    assert_eq!(done.quota_decision, QuotaDecision::Allow);

    assert_untouched(&rig, &chat, &before).await;
    assert_eq!(
        rig.gateway.total_calls(),
        gateway_calls,
        "no preflight, no policy read"
    );
    assert!(
        rig.quota.metrics.calls().is_empty(),
        "no quota metric of any kind"
    );
    assert!(
        crate::test_support::chats::outbox_rows(&rig.quota.db)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_replay_is_served_while_another_turn_of_the_chat_is_running() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, _) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    let outcome = send(&rig, &chat, with_request(plain("q"), request_id))
        .await
        .unwrap();
    assert!(matches!(outcome, SendOutcome::Replay(_)), "{outcome:?}");
}

#[tokio::test]
async fn a_replay_after_a_downgrade_names_the_selected_model() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let request_id = Uuid::new_v4();
    let assistant = Uuid::new_v4();
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "q", t(10)).request(Some(request_id)),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "a", t(11))
            .id(assistant)
            .request(Some(request_id))
            .model(STANDARD)
            .tokens(5, 6),
    )
    .await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed")
            .request(request_id)
            .assistant(assistant)
            .model(STANDARD),
    )
    .await;
    let outcome = send(&rig, &chat, with_request(plain("q"), request_id))
        .await
        .unwrap();
    let SendOutcome::Replay(events) = outcome else {
        panic!("{outcome:?}")
    };
    let StreamEvent::Done(done) = &events[2] else {
        panic!("{events:?}")
    };
    assert_eq!(done.selected_model, PREMIUM);
    assert_eq!(done.effective_model, STANDARD);
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(done.downgrade_from.as_deref(), Some(PREMIUM));
}

// ── the turn admission checks ────────────────────────────────────────────────

#[tokio::test]
async fn reusing_a_request_id_in_any_other_state_is_a_request_id_conflict() {
    for (label, seed) in [
        ("running", TurnSeedFull::new("running")),
        (
            "failed",
            TurnSeedFull::new("failed").error("provider_error"),
        ),
        ("cancelled", TurnSeedFull::new("cancelled")),
        (
            "completed but soft-deleted",
            TurnSeedFull::new("completed").deleted(t(5)),
        ),
        (
            "failed and soft-deleted",
            TurnSeedFull::new("failed").deleted(t(5)),
        ),
    ] {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        let request_id = seed.request_id;
        seed_turn_full(&rig.quota.db, &chat, seed).await;
        let before = footprint(&rig.quota.db, &chat).await;
        let error = error_of(send(&rig, &chat, with_request(plain("again"), request_id)).await);
        assert!(
            matches!(
                error,
                DomainError::Conflict {
                    reason: ConflictReason::RequestIdConflict
                }
            ),
            "{label}: {error:?}"
        );
        assert_untouched(&rig, &chat, &before).await;
        assert!(
            rig.quota.metrics.calls().is_empty(),
            "{label}: admission runs before the preflight"
        );
    }
}

#[tokio::test]
async fn a_second_turn_while_one_is_running_is_turn_already_running_with_any_or_no_request_id() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    for send_request in [
        plain("no key"),
        with_request(plain("other key"), Uuid::new_v4()),
    ] {
        let error = error_of(send(&rig, &chat, send_request).await);
        assert!(
            matches!(
                error,
                DomainError::Conflict {
                    reason: ConflictReason::TurnAlreadyRunning
                }
            ),
            "{error:?}"
        );
    }
    assert_untouched(&rig, &chat, &before).await;
    assert_eq!(
        rig.gateway.total_calls(),
        0,
        "the guard runs before the quota preflight"
    );
}

#[tokio::test]
async fn a_new_turn_is_accepted_once_the_previous_one_is_terminal() {
    for state in ["completed", "failed", "cancelled"] {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new(state)).await;
        let outcome = send(&rig, &chat, plain("next")).await;
        assert!(
            matches!(outcome, Ok(SendOutcome::Streaming(_))),
            "{state}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn a_second_send_after_a_started_turn_is_rejected_until_it_is_terminal() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    plan_of(send(&rig, &chat, plain("first")).await);
    let error = error_of(send(&rig, &chat, plain("second")).await);
    assert!(matches!(
        error,
        DomainError::Conflict {
            reason: ConflictReason::TurnAlreadyRunning
        }
    ));
    finish_running_turns(&rig, &chat).await;
    assert!(matches!(
        send(&rig, &chat, plain("third")).await,
        Ok(SendOutcome::Streaming(_))
    ));
}

// ── admission and the chat ───────────────────────────────────────────────────

#[tokio::test]
async fn admission_asks_the_pdp_for_send_message_only() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    plan_of(send(&rig, &chat, plain("Hello")).await);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "send_message");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_not_found_and_nothing_is_written() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let foreign_user = rig.quota.chat(&a2(), STANDARD).await;
    let before = footprint(&rig.quota.db, &chat).await;
    for (who, target) in [(a2(), &chat), (b1(), &chat), (a1(), &foreign_user)] {
        let error = error_of(send_as(&rig, &who, target, plain("hi")).await);
        assert!(
            matches!(
                error,
                DomainError::NotFound {
                    resource: ResourceKind::Chat,
                    ..
                }
            ),
            "{error:?}"
        );
    }
    let unknown = error_of(
        rig.service
            .send(
                &a1(),
                Uuid::new_v4(),
                plain("hi"),
                &CancellationToken::new(),
            )
            .await,
    );
    assert!(matches!(
        unknown,
        DomainError::NotFound {
            resource: ResourceKind::Chat,
            ..
        }
    ));
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn a_denied_admission_is_refused_before_any_read() {
    let rig = send_rig_with(SendRigOptions::new().with_pdp(crate::test_support::Pdp::Deny)).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
    assert_untouched(&rig, &chat, &before).await;
    assert_eq!(rig.gateway.total_calls(), 0);
}

// ── preflight rejections ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_model_missing_from_the_catalog_is_invalid_model_before_the_quota_preflight() {
    let rig = send_rig().await;
    let chat = rig.quota.chat_a1("ghost-model").await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(
        matches!(error, DomainError::InvalidModel { .. }),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
    assert_eq!(preflight_calls(&rig), 0);
}

#[tokio::test]
async fn a_failing_policy_plugin_is_an_internal_failure_with_nothing_written() {
    let rig = send_rig().await;
    rig.gateway
        .fail_current
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(
        matches!(
            error,
            DomainError::PolicyUnavailable { .. } | DomainError::Internal { .. }
        ),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn too_many_images_are_rejected_before_the_quota_preflight() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await);
    }
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_attachments(plain("pics"), &ids)).await);
    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
    assert_untouched(&rig, &chat, &before).await;
    assert_eq!(
        preflight_calls(&rig),
        0,
        "the image count is checked before the quota"
    );
}

#[tokio::test]
async fn the_image_count_wins_over_the_vision_check() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await);
    }
    let error = error_of(send(&rig, &chat, with_attachments(plain("pics"), &ids)).await);
    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
}

#[tokio::test]
async fn an_exhausted_token_quota_is_a_tokens_rejection_with_no_reserve() {
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
    assert!(reserve_calls(&rig).is_empty());
}

#[tokio::test]
async fn the_quota_rejection_wins_over_the_input_limit() {
    let rig = rig_with_std(&json!({ "max_input_tokens": 50 })).await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let error = error_of(send(&rig, &chat, plain(&"y".repeat(400))).await);
    assert!(
        matches!(error, DomainError::QuotaExceeded { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_message_above_max_input_tokens_is_input_too_long_and_one_at_the_limit_is_not() {
    // no system prompt: the system instructions count toward the 50-token input limit of the
    // context budget, so a prompt would leave no room for a message exactly at the limit
    let rig = rig_with_std(&json!({ "max_input_tokens": 50, "system_prompt": "" })).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    // four bytes per token: 400 bytes estimate 100 tokens, 200 bytes exactly 50
    let error = error_of(send(&rig, &chat, plain(&"y".repeat(400))).await);
    assert!(matches!(error, DomainError::InputTooLong), "{error:?}");
    assert_untouched(&rig, &chat, &before).await;
    plan_of(send(&rig, &chat, plain(&"y".repeat(200))).await);
}

#[tokio::test]
async fn the_input_limit_is_the_one_of_the_effective_model_after_a_downgrade() {
    let rig = rig_with_std(&json!({ "max_input_tokens": 50 })).await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let error = error_of(send(&rig, &chat, plain(&"y".repeat(400))).await);
    assert!(matches!(error, DomainError::InputTooLong), "{error:?}");
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_not_supported() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_attachments(plain("see"), &[image])).await);
    assert!(
        matches!(error, DomainError::VisionNotSupported),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
    // a text-only request on the same model is never rejected
    assert!(matches!(
        send(&rig, &chat, plain("text only")).await,
        Ok(SendOutcome::Streaming(_))
    ));
}

#[tokio::test]
async fn images_after_a_downgrade_to_a_model_without_vision_are_not_supported() {
    let rig = send_rig().await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_attachments(plain("see"), &[image])).await);
    assert!(
        matches!(error, DomainError::VisionNotSupported),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn the_disable_images_kill_switch_rejects_images_but_not_text() {
    let mut snapshot = default_catalog();
    snapshot.kill_switches.disable_images = true;
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(snapshot)).await;
    let chat = rig.prem_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, with_attachments(plain("see"), &[image])).await);
    assert!(
        matches!(error, DomainError::FeatureDisabled { subject: "images" }),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
    assert!(matches!(
        send(&rig, &chat, plain("text")).await,
        Ok(SendOutcome::Streaming(_))
    ));
}

#[tokio::test]
async fn mandatory_context_that_does_not_fit_is_a_context_budget_rejection() {
    // window 600, output cap 500: an input limit of 100 bytes at one byte per token
    let rig = rig_with_std(&json!({
        "context_window": 600, "max_input_tokens": 0,
        "estimation_budgets": byte_budgets(),
    }))
    .await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain(&"z".repeat(150))).await);
    assert!(
        matches!(error, DomainError::ContextBudgetExceeded),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn an_output_cap_at_or_above_the_window_is_a_context_budget_rejection() {
    let rig = rig_with_std(&json!({ "context_window": 400, "max_input_tokens": 0 })).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(
        matches!(error, DomainError::ContextBudgetExceeded),
        "{error:?}"
    );
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn an_unresolvable_provider_is_an_internal_failure_with_nothing_written() {
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({ "provider_id": "ghost_provider" })),
    ])))
    .await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
    assert_untouched(&rig, &chat, &before).await;
}

#[tokio::test]
async fn an_attachment_that_fails_validation_rolls_back_the_reserve_and_never_reaches_the_provider()
{
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let good = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let cases = [
        ("unknown", Uuid::new_v4()),
        (
            "foreign uploader",
            seed_att(
                &rig.quota.db,
                &chat,
                AttachmentSeed::ready("document").uploaded_by(crate::test_support::USER_A2),
            )
            .await,
        ),
        (
            "not ready",
            seed_att(
                &rig.quota.db,
                &chat,
                AttachmentSeed::ready("document").status("uploaded"),
            )
            .await,
        ),
        (
            "deleted",
            seed_att(
                &rig.quota.db,
                &chat,
                AttachmentSeed::ready("document").deleted(at(3)),
            )
            .await,
        ),
    ];
    let before = footprint(&rig.quota.db, &chat).await;
    for (what, bad) in cases {
        let error =
            error_of(send(&rig, &chat, with_attachments(plain("files"), &[good, bad])).await);
        assert!(
            matches!(error, DomainError::InvalidAttachment),
            "{what}: {error:?}"
        );
        assert_untouched(&rig, &chat, &before).await;
        assert!(
            reserve_calls(&rig).is_empty(),
            "{what}: no reserve metric without a commit"
        );
    }
}

// ── the pre-provider cancellation check ──────────────────────────────────────

#[tokio::test]
async fn a_token_cancelled_after_the_reserve_commit_sends_no_provider_request() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = rig
        .service
        .send(&a1(), chat.id, plain("hi"), &cancel)
        .await
        .unwrap();
    let SendOutcome::CancelledBeforeProvider(reserved) = outcome else {
        panic!("expected a pre-provider cancellation, got {outcome:?}");
    };
    assert_eq!(rig.provider.call_count(), 0, "no provider task was started");
    // the setup that already committed is not interrupted: the turn and its user message exist
    // and the turn was finalized `cancelled` before the setup returned
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, reserved.turn_id);
    assert_eq!(turns[0].state, "cancelled");
    assert_eq!(turns[0].error_code, None);
    assert_eq!(turns[0].assistant_message_id, None);
    let messages = crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(messages.len(), 1, "no assistant message");
    assert_eq!(messages[0].id, reserved.user_message_id);
    // the reserve is settled on the estimated path with one `aborted` usage event
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].terminal_state, "cancelled");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
}

#[tokio::test]
async fn a_provider_task_that_cannot_start_leaves_a_failed_turn_and_no_reserve() {
    let rig = send_rig_with(SendRigOptions::new().with_provider(ProviderMode::Failing)).await;
    let chat = rig.std_chat().await;
    let error = error_of(send(&rig, &chat, plain("hi")).await);
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
    assert_eq!(rig.provider.call_count(), 1, "the task was asked to start");
    // never a `running` turn: it ended `failed` with `provider_error`, estimated, reserve settled
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, "failed");
    assert_eq!(turns[0].error_code.as_deref(), Some("provider_error"));
    assert_eq!(turns[0].assistant_message_id, None);
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].billing_outcome, "failed");
    assert_eq!(events[0].settlement_method, "estimated");
    // the chat accepts the next request at once (the admission checks do not answer 409)
    let second = error_of(send(&rig, &chat, plain("again")).await);
    assert!(matches!(second, DomainError::Internal { .. }), "{second:?}");
    let states: Vec<String> = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .map(|turn| turn.state)
        .collect();
    assert_eq!(states, vec!["failed", "failed"]);
}

#[tokio::test]
async fn a_live_token_lets_the_provider_task_start() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let cancel = CancellationToken::new();
    let outcome = rig
        .service
        .send(&a1(), chat.id, plain("hi"), &cancel)
        .await
        .unwrap();
    assert!(matches!(outcome, SendOutcome::Streaming(_)));
    assert_eq!(rig.provider.call_count(), 1);
}

#[tokio::test]
async fn a_cancelled_token_does_not_turn_a_rejected_request_into_anything_else() {
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = error_of(rig.service.send(&a1(), chat.id, plain("hi"), &cancel).await);
    assert!(
        matches!(error, DomainError::QuotaExceeded { .. }),
        "{error:?}"
    );
}

// ── validation and settings ──────────────────────────────────────────────────

#[tokio::test]
async fn the_service_validates_with_the_limits_of_its_settings() {
    let rig = send_rig().await;
    let ok = rig
        .service
        .validate(RawSendBody {
            content: "hi".to_owned(),
            request_id: None,
            attachment_ids: None,
            web_search_enabled: None,
        })
        .unwrap();
    assert_eq!(ok.content, "hi");
    let empty = rig.service.validate(RawSendBody {
        content: "  ".to_owned(),
        request_id: None,
        attachment_ids: None,
        web_search_enabled: None,
    });
    assert!(matches!(empty, Err(DomainError::EmptyContent)));
    let crowded = rig.service.validate(RawSendBody {
        content: "hi".to_owned(),
        request_id: None,
        attachment_ids: Some((0..55).map(|_| Uuid::new_v4()).collect()),
        web_search_enabled: None,
    });
    assert!(matches!(crowded, Err(DomainError::InvalidAttachment)));
}

#[test]
fn the_settings_come_from_the_validated_configuration() {
    let defaults = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" }
    }))
    .unwrap();
    let settings = StreamSettings::from_config(&defaults);
    assert_eq!(settings.recent_messages_limit, 10);
    assert_eq!(settings.max_images_per_message, 4);
    assert_eq!(settings.max_documents_per_chat, 50);
    assert_eq!(settings.web_search_guard, defaults.context.web_search_guard);
    assert_eq!(
        settings.file_search_guard,
        defaults.context.file_search_guard
    );

    let tuned = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" },
        "context": { "recent_messages_limit": 3 },
        "rag": { "max_images_per_message": 2, "max_documents_per_chat": 7 }
    }))
    .unwrap();
    let settings = StreamSettings::from_config(&tuned);
    assert_eq!(settings.recent_messages_limit, 3);
    assert_eq!(settings.max_images_per_message, 2);
    assert_eq!(settings.max_documents_per_chat, 7);
    let _ = FILE_GUARD;
}
