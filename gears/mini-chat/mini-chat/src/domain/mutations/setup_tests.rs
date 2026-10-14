//! The mutation flow after the commit over in-memory `SQLite`, the real quota service and the
//! recording provider-task port: the plan handed to the send pipeline (new server-generated
//! `request_id`, pre-allocated assistant message id, context before the mutated turn, carried
//! images and web search flag, tools decided afresh), the reserve through `ReserveTurn::Existing`
//! before the first provider byte, the pre-provider cancellation, and every post-commit setup
//! failure leaving a `failed` replacement turn without settlement, usage event or turn audit
//! event.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flows
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn` (`inst-retry-new-request-id`,
//! `inst-retry-commit`, `inst-retry-setup`, `inst-retry-setup-failed`, `inst-retry-return-setup`,
//! `inst-retry-dropped`, `inst-retry-return-dropped`, `inst-retry-stream`) and
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn` (`inst-edit-new-request-id`,
//! `inst-edit-setup`, `inst-edit-dropped`, `inst-edit-stream`); algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup` (`inst-pcs-no-pdp`,
//! `inst-pcs-tools`, `inst-pcs-context`, `inst-pcs-context-rejected`, `inst-pcs-return-context`,
//! `inst-pcs-provider`, `inst-pcs-provider-failed`, `inst-pcs-return-provider`,
//! `inst-pcs-message-id`, `inst-pcs-reserve`, `inst-pcs-over-limit`, `inst-pcs-return-429`,
//! `inst-pcs-commit`, `inst-pcs-before-provider`, `inst-pcs-handoff`, `inst-pcs-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure` (`inst-uf-cas`,
//! `inst-uf-no-settlement`, `inst-uf-keep-replacement`, `inst-uf-update-failed`,
//! `inst-uf-orphan-fallback`, `inst-uf-not-sse`, `inst-uf-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward`
//! (`inst-tcf-counters`, `inst-tcf-no-refund`, `inst-tcf-tools-afresh`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`
//! (`inst-ae-replacement-turn`), flow
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability` (`inst-obs-logs`,
//! `inst-obs-return`) and `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics`
//! (`inst-met-cardinality`: no content in logs); state
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`
//! (`inst-rts-to-reserved`, `inst-rts-to-failed-setup`, `inst-rts-to-failed-orphan`,
//! `inst-rts-immutable`); definitions of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-retry`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-post-commit-setup` and
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;
use uuid::Uuid;

use super::{MutationOp, ValidatedEdit};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::streaming::{ProviderCallPlan, SendOutcome, ToolKind};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, message_rows, outbox_body_count};
use crate::test_support::mutations::{
    SeededTurn, live_messages, mutation_events, replacement_of, seed, snapshot,
    soft_delete_attachment, turn, turn_by_request, turn_events,
};
use crate::test_support::quota::{
    FixedClock, STANDARD, all_quota_rows, default_now, reserved_of, row, seed_rows,
};
use crate::test_support::send::{
    AttachmentSeed, SendRig, SendRigOptions, byte_budgets, catalog_of, link_rows, prem_model,
    seed_att, send_rig, send_rig_with, std_model, turn_rows, with_web_search,
};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

fn plan_of(outcome: Result<SendOutcome, DomainError>) -> ProviderCallPlan {
    match outcome {
        Ok(SendOutcome::Streaming(started)) => started.plan,
        other => panic!("expected a started replacement turn, got {other:?}"),
    }
}

async fn retry(
    rig: &SendRig,
    chat: &ChatRecord,
    seeded: &SeededTurn,
) -> Result<SendOutcome, DomainError> {
    rig.mutations
        .retry(&a1(), chat.id, seeded.request_id, &CancellationToken::new())
        .await
}

async fn edit(
    rig: &SendRig,
    chat: &ChatRecord,
    seeded: &SeededTurn,
    content: &str,
) -> Result<SendOutcome, DomainError> {
    rig.mutations
        .edit(
            &a1(),
            chat.id,
            seeded.request_id,
            ValidatedEdit {
                content: content.to_owned(),
            },
            &CancellationToken::new(),
        )
        .await
}

async fn rig_with_std(patch: &serde_json::Value) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_snapshot(catalog_of(vec![prem_model(&json!({})), std_model(patch)])),
    )
    .await
}

// ── the plan handed to the send pipeline ─────────────────────────────────────

#[tokio::test]
async fn a_retry_hands_a_plan_with_a_new_server_generated_request_id() {
    // inst-retry-new-request-id, inst-pcs-message-id, inst-pcs-handoff, inst-pcs-return
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("what is rust?")).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);

    assert_ne!(plan.request_id, seeded.request_id);
    assert_eq!(plan.request_id.get_version_num(), 4, "a UUID v4");
    assert_eq!(plan.chat_id, chat.id);
    assert_eq!(plan.user_id, crate::test_support::USER_A1);
    assert_eq!(plan.tenant_id, crate::test_support::TENANT_A);
    assert_eq!(plan.selected_model, STANDARD);
    assert_eq!(plan.assistant_message_id.get_version_num(), 4);
    assert_ne!(Some(plan.assistant_message_id), seeded.assistant_message_id);
    // no assistant row exists until finalization
    let assistants = message_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .filter(|m| m.role == "assistant" && m.id == plan.assistant_message_id)
        .count();
    assert_eq!(assistants, 0);
    assert_eq!(
        rig.provider.call_count(),
        1,
        "the provider task was started once"
    );
}

#[tokio::test]
async fn the_plan_turn_is_the_committed_replacement_turn_and_message() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(replacement.id, plan.turn_id);
    assert_eq!(replacement.request_id, plan.request_id);
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 1, "only the replacement user message is live");
    assert_eq!(live[0].id, plan.user_message_id);
    assert_eq!(live[0].request_id, Some(plan.request_id));
    assert_eq!(live[0].content, "question");
}

#[tokio::test]
async fn an_edit_sends_the_content_exactly_as_given() {
    // inst-edit-new-request-id, inst-edit-setup, inst-ev-content-kept
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("old")).await;
    let plan = plan_of(edit(&rig, &chat, &seeded, "  new   text \n").await);
    assert_eq!(plan.context.plan.user.text, "  new   text \n");
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live[0].content, "  new   text \n");
    assert_ne!(plan.request_id, seeded.request_id);
}

#[tokio::test]
async fn the_context_holds_the_history_before_the_mutated_turn_only() {
    // inst-pcs-context, inst-mpf-snapshot-boundary
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let first = seed(
        &rig.quota.db,
        &chat,
        turn("first question").at(10).answer("first answer"),
    )
    .await;
    let second = seed(
        &rig.quota.db,
        &chat,
        turn("second question").at(100).answer("second answer"),
    )
    .await;
    let plan = plan_of(retry(&rig, &chat, &second).await);
    let texts: Vec<&str> = plan
        .context
        .plan
        .recent
        .iter()
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(texts, vec!["first question", "first answer"]);
    assert_eq!(plan.context.plan.user.text, "second question");
    let ids: Vec<Option<Uuid>> = plan
        .context
        .plan
        .recent
        .iter()
        .map(|i| i.message_id)
        .collect();
    assert_eq!(
        ids,
        vec![Some(first.user_message_id), first.assistant_message_id]
    );
}

#[tokio::test]
async fn the_first_turn_of_a_chat_has_an_empty_history() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let only = seed(&rig.quota.db, &chat, turn("only")).await;
    let plan = plan_of(retry(&rig, &chat, &only).await);
    assert!(plan.context.plan.recent.is_empty());
    assert!(plan.context.plan.thread_summary.is_none());
}

#[tokio::test]
async fn the_carried_images_are_the_current_user_item_in_link_order() {
    // inst-acf-classify, inst-pcs-context
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let late = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-late"),
    )
    .await;
    let early = seed_att(
        db,
        &chat,
        AttachmentSeed::ready("image").file_id("file-early"),
    )
    .await;
    let document = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let seeded = seed(
        db,
        &chat,
        turn("see")
            .attach(late, 30)
            .attach(early, 20)
            .attach(document, 40),
    )
    .await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let files: Vec<&str> = plan
        .context
        .plan
        .user
        .images
        .iter()
        .map(|image| image.provider_file_id.as_str())
        .collect();
    assert_eq!(files, vec!["file-early", "file-late"]);
}

#[tokio::test]
async fn the_web_search_tool_follows_the_carried_flag_and_the_model_support() {
    // inst-tcf-web-search-flag, inst-tcf-tools-afresh, inst-pcs-tools
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let with_flag = seed(&rig.quota.db, &chat, turn("q").web_search()).await;
    let plan = plan_of(retry(&rig, &chat, &with_flag).await);
    assert!(plan.web_search_enabled);
    assert!(
        plan.tools
            .tools
            .iter()
            .any(|t| t.kind == ToolKind::WebSearch)
    );

    let other = rig.std_chat().await;
    let without_flag = seed(&rig.quota.db, &other, turn("q")).await;
    let plan = plan_of(retry(&rig, &other, &without_flag).await);
    assert!(!plan.web_search_enabled);
    assert!(plan.tools.tools.is_empty());
}

#[tokio::test]
async fn the_flag_is_kept_without_the_tool_when_the_model_has_no_web_search_support() {
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(catalog_of(vec![
        prem_model(&json!({})),
        with_web_search(&std_model(&json!({})), false),
    ])))
    .await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q").web_search()).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    assert!(plan.web_search_enabled, "the carried flag is kept");
    assert!(
        !plan
            .tools
            .tools
            .iter()
            .any(|t| t.kind == ToolKind::WebSearch)
    );
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert!(replacement.web_search_enabled);
}

// ── the reserve through `ReserveTurn::Existing` ──────────────────────────────

#[tokio::test]
async fn the_replacement_turn_is_reserved_through_the_existing_path() {
    // inst-pcs-reserve, inst-pcs-commit, inst-rts-to-reserved: the preflight columns are filled
    // once and `started_at` is the preflight instant
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);

    let replacement = turn_by_request(&rig.quota.db, chat.id, plan.request_id).await;
    assert_eq!(replacement.state, "running");
    assert_eq!(replacement.started_at, plan.decision.started_at);
    assert_eq!(replacement.started_at, default_now());
    assert_eq!(
        replacement.reserve_tokens,
        Some(plan.decision.reserve_tokens)
    );
    assert_eq!(
        replacement.reserved_credits_micro,
        Some(plan.decision.reserved_credits_micro)
    );
    assert_eq!(
        replacement.max_output_tokens_applied,
        Some(i32::try_from(plan.decision.max_output_tokens_applied).unwrap())
    );
    assert_eq!(
        replacement.effective_model.as_deref(),
        Some(plan.decision.effective_model.as_str())
    );
    assert_eq!(
        replacement.policy_version_applied,
        Some(i64::try_from(plan.decision.policy_version_applied).unwrap())
    );
    assert_eq!(
        replacement.minimal_generation_floor_applied,
        Some(i32::try_from(plan.decision.minimal_generation_floor_applied).unwrap())
    );
    // the buckets are booked once, on the periods of the preflight instant
    let credits = plan.decision.reserved_credits_micro;
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        credits
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "monthly", MONTH, "total").await,
        credits
    );
}

#[tokio::test]
async fn the_reserve_commits_before_the_first_provider_byte() {
    // inst-pcs-before-provider: constraint quota-before-outbound
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    retry(&rig, &chat, &seeded).await.unwrap();
    let observations = rig.provider.observations();
    assert_eq!(observations.len(), 1);
    assert_eq!(
        observations[0].running_turns, 1,
        "the replacement turn is running"
    );
    assert!(
        observations[0].reserved_credits > 0,
        "the reserve was committed before the provider task started"
    );
}

#[tokio::test]
async fn the_replacement_turn_starts_with_zero_counters_and_the_replaced_keeps_its_own() {
    // inst-tcf-counters, inst-tcf-no-refund
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q").web_search().counts(2, 1, 3)).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let replacement = turn_by_request(&rig.quota.db, chat.id, plan.request_id).await;
    assert_eq!(
        (
            replacement.web_search_completed_count,
            replacement.code_interpreter_completed_count,
            replacement.file_search_completed_count
        ),
        (0, 0, 0)
    );
    let replaced = turn_by_request(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(
        (
            replaced.web_search_completed_count,
            replaced.code_interpreter_completed_count,
            replaced.file_search_completed_count
        ),
        (2, 1, 3)
    );
}

#[tokio::test]
async fn a_mutation_never_refunds_or_re_settles_the_replaced_turn() {
    // inst-tcf-no-refund: the replaced turn was settled once; its usage event is not repeated
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    seed_rows(
        &rig.quota.db,
        &[
            row("daily", DAY, "total").spent(5_000),
            row("monthly", MONTH, "total").spent(5_000),
        ],
    )
    .await;
    let before_events = rig.events().await.len();
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let rows = all_quota_rows(&rig.quota.db).await;
    assert!(
        rows.iter().all(|r| r.spent == 5_000),
        "spent is untouched: {rows:?}"
    );
    assert_eq!(
        rig.events().await.len(),
        before_events,
        "no usage event of the mutation"
    );
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        plan.decision.reserved_credits_micro
    );
}

#[tokio::test]
async fn the_requester_of_the_replacement_turn_is_the_caller() {
    // ADR-0003: the mutating user is the requester and is charged
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let replacement = turn_by_request(&rig.quota.db, chat.id, plan.request_id).await;
    assert_eq!(
        replacement.requester_user_id,
        Some(crate::test_support::USER_A1)
    );
    assert_eq!(replacement.requester_type, "user");
    let quota_rows = all_quota_rows(&rig.quota.db).await;
    assert!(
        quota_rows
            .iter()
            .all(|r| r.user_id == crate::test_support::USER_A1),
        "{quota_rows:?}"
    );
}

#[tokio::test]
async fn a_whole_retry_evaluates_the_pdp_exactly_once() {
    // inst-pcs-no-pdp: the post-commit setup reuses the admission scope
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    retry(&rig, &chat, &seeded).await.unwrap();
    assert_eq!(rig.pdp.only_request().action.name, "retry_turn");

    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    edit(&rig, &chat, &seeded, "changed").await.unwrap();
    assert_eq!(rig.pdp.only_request().action.name, "edit_turn");
}

// ── the pre-provider cancellation ────────────────────────────────────────────

#[tokio::test]
async fn a_token_cancelled_after_the_reserve_finalizes_the_turn_cancelled_without_a_provider_call()
{
    // inst-retry-dropped, inst-retry-return-dropped, inst-edit-dropped
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = rig
        .mutations
        .retry(&a1(), chat.id, seeded.request_id, &cancel)
        .await
        .unwrap();
    let SendOutcome::CancelledBeforeProvider(reserved) = outcome else {
        panic!("expected a pre-provider cancellation, got {outcome:?}");
    };
    assert_eq!(rig.provider.call_count(), 0, "no provider request");
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(replacement.id, reserved.turn_id);
    assert_eq!(replacement.state, "cancelled");
    // estimated settlement, one `aborted` usage event and one `turn_failed` audit event
    let events = rig.events().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(turn_events(&rig).await.len(), 1);
}

// ── post-commit setup failures ───────────────────────────────────────────────

/// The turn rows after a setup failure: the replacement turn is terminal under `code` with a
/// sanitized detail, the previous turn stays replaced and the replacement user message stays.
async fn assert_failed_turn_rows(
    rig: &SendRig,
    chat: &ChatRecord,
    seeded: &SeededTurn,
    code: &str,
) {
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(
        replacement.state, "failed",
        "the replacement turn is terminal"
    );
    assert_eq!(replacement.error_code.as_deref(), Some(code));
    assert!(replacement.completed_at.is_some());
    let detail = replacement.error_detail.clone().unwrap_or_default();
    assert!(!detail.is_empty(), "a sanitized detail is stored");
    assert!(!detail.contains(&seeded.request_id.to_string()), "{detail}");
    assert!(replacement.deleted_at.is_none());
    let previous = turn_by_request(&rig.quota.db, chat.id, seeded.request_id).await;
    assert!(previous.deleted_at.is_some());
    assert_eq!(
        previous.replaced_by_request_id,
        Some(replacement.request_id)
    );
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].request_id, Some(replacement.request_id));
}

/// What an unstarted replacement turn must look like after a setup failure.
async fn assert_unstarted_failure(
    rig: &SendRig,
    chat: &ChatRecord,
    seeded: &SeededTurn,
    code: &str,
) {
    assert_failed_turn_rows(rig, chat, seeded, code).await;
    // no settlement, no usage event, no turn audit event; the mutation audit event stays
    assert!(rig.events().await.is_empty(), "no usage event");
    assert!(turn_events(rig).await.is_empty(), "no turn audit event");
    assert_eq!(mutation_events(rig).await.len(), 1);
    // the chat is not blocked: nothing is running and a new turn starts at once
    assert!(
        turn_rows(&rig.quota.db, chat.id)
            .await
            .iter()
            .all(|turn| turn.state != "running")
    );
}

#[tokio::test]
async fn a_context_that_no_longer_fits_fails_the_replacement_turn_with_context_length_exceeded() {
    // inst-pcs-context-rejected, inst-pcs-return-context, inst-uf-cas, inst-uf-keep-replacement,
    // inst-rts-to-failed-setup
    let rig = rig_with_std(&json!({
        "context_window": 600, "max_input_tokens": 0, "estimation_budgets": byte_budgets(),
    }))
    .await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn(&"z".repeat(150))).await;
    let result = retry(&rig, &chat, &seeded).await;
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
    assert_unstarted_failure(&rig, &chat, &seeded, "context_length_exceeded").await;
    assert!(
        all_quota_rows(&rig.quota.db).await.is_empty(),
        "no reserve was taken"
    );
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn an_unresolvable_provider_fails_the_replacement_turn_with_turn_setup_failed() {
    // inst-pcs-provider-failed, inst-pcs-return-provider
    let rig = rig_with_std(&json!({ "provider_id": "ghost_provider" })).await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let result = edit(&rig, &chat, &seeded, "rephrased").await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_unstarted_failure(&rig, &chat, &seeded, "turn_setup_failed").await;
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn a_reserve_re_check_over_the_limit_fails_the_turn_with_quota_exceeded() {
    // inst-pcs-over-limit, inst-pcs-return-429: a concurrent reserve filled the quota between
    // the preflight and the reserve
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    let detail = rig
        .mutations
        .admit(&a1(), MutationOp::Retry, chat.id)
        .await
        .unwrap();
    let preview = rig
        .mutations
        .preview(&a1(), &detail, MutationOp::Retry, seeded.request_id)
        .await
        .unwrap();
    let prepared = rig
        .mutations
        .preflight(&a1(), &detail, preview, "question".to_owned())
        .await
        .unwrap();
    let credits = prepared.decision.reserved_credits_micro;
    seed_rows(
        &rig.quota.db,
        &[row("daily", DAY, "total").reserved(60_000_000 - credits + 1)],
    )
    .await;
    let committed = rig
        .mutations
        .commit(&a1(), &detail, &prepared)
        .await
        .unwrap();
    let result = rig
        .mutations
        .setup(
            &a1(),
            &detail,
            prepared,
            &committed,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            })
        ),
        "{result:?}"
    );
    assert_unstarted_failure(&rig, &chat, &seeded, "quota_exceeded").await;
    assert_eq!(
        reserved_of(&rig.quota.db, "daily", DAY, "total").await,
        60_000_000 - credits + 1,
        "the increment of the failed reserve rolled back"
    );
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn the_failed_replacement_turn_can_be_retried_deleted_or_followed_by_a_new_send() {
    // inst-uf-keep-replacement: the client can retry, edit or delete the failed replacement turn
    let rig = rig_with_std(&json!({ "provider_id": "ghost_provider" })).await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    drop(retry(&rig, &chat, &seeded).await);
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    let detail = rig
        .mutations
        .admit(&a1(), MutationOp::Retry, chat.id)
        .await
        .unwrap();
    let preview = rig
        .mutations
        .preview(&a1(), &detail, MutationOp::Retry, replacement.request_id)
        .await
        .expect("the failed replacement turn is the latest terminal turn");
    assert_eq!(preview.target.row.request_id, replacement.request_id);
}

#[tokio::test]
async fn a_failure_of_the_failure_update_leaves_the_turn_running_for_the_orphan_watchdog() {
    // inst-uf-update-failed, inst-uf-orphan-fallback, inst-rts-to-failed-orphan
    let clock = FixedClock::at(default_now());
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_clock(clock.clone())
            .with_snapshot(catalog_of(vec![
                prem_model(&json!({})),
                std_model(&json!({ "provider_id": "ghost_provider" })),
            ])),
    )
    .await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question")).await;
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_unstarted_cas BEFORE UPDATE OF state ON chat_turns \
         WHEN NEW.state = 'failed' BEGIN SELECT RAISE(ABORT, 'forced cas failure'); END",
    )
    .await
    .unwrap();
    let result = retry(&rig, &chat, &seeded).await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(replacement.state, "running", "the CAS could not be written");
    assert_eq!(replacement.reserve_tokens, None, "no reserve exists");

    crate::test_support::raw_exec(&rig.quota.db.raw, "DROP TRIGGER fail_unstarted_cas")
        .await
        .unwrap();
    clock.set(default_now() + time::Duration::seconds(301));
    let report = rig
        .watchdog(std::time::Duration::from_secs(300))
        .scan_once()
        .await
        .unwrap();
    assert_eq!(report.finalized, 1, "{report:?}");
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert_eq!(replacement.state, "failed");
    assert_eq!(replacement.error_code.as_deref(), Some("orphan_timeout"));
    assert!(
        all_quota_rows(&rig.quota.db).await.is_empty(),
        "no settlement of a missing reserve"
    );
}

// ── the commit, seen from the setup ──────────────────────────────────────────

#[tokio::test]
async fn the_commit_carries_the_attachment_links_of_the_original_message() {
    // inst-mc-copy-links through the service: the original rows stay, new rows are written
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let db = &rig.quota.db;
    let image = seed_att(db, &chat, AttachmentSeed::ready("image")).await;
    let gone = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let seeded = seed(db, &chat, turn("files").attach(image, 10).attach(gone, 11)).await;
    soft_delete_attachment(db, gone).await;
    let plan = plan_of(retry(&rig, &chat, &seeded).await);
    let links = link_rows(db, chat.id).await;
    let on = |message: Uuid| -> Vec<Uuid> {
        links
            .iter()
            .filter(|l| l.message_id == message)
            .map(|l| l.attachment_id)
            .collect()
    };
    assert_eq!(
        on(seeded.user_message_id).len(),
        2,
        "the original rows stay"
    );
    assert_eq!(
        on(plan.user_message_id),
        vec![image],
        "the deleted one is excluded"
    );
}

#[tokio::test]
async fn nothing_but_the_mutation_audit_event_is_enqueued_by_a_successful_setup() {
    // the replacement turn adds its own events only at its finalization
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = outbox_body_count(&rig.quota.db).await;
    retry(&rig, &chat, &seeded).await.unwrap();
    assert_eq!(outbox_body_count(&rig.quota.db).await, before + 1);
    assert_eq!(mutation_events(&rig).await.len(), 1);
    assert!(turn_events(&rig).await.is_empty());
    assert!(rig.events().await.is_empty());
}

#[tokio::test]
async fn a_rejected_preflight_through_the_whole_flow_leaves_the_chat_unchanged() {
    // inst-retry-return-preflight: the previous turn, its answer and the chat stay unchanged
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = retry(&rig, &chat, &seeded).await;
    assert!(
        matches!(result, Err(DomainError::QuotaExceeded { .. })),
        "{result:?}"
    );
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
}

#[traced_test]
#[tokio::test]
async fn a_setup_failure_is_logged_with_the_ids_and_the_error_code_never_the_message_content() {
    // inst-obs-logs: the admin correlates a failed replacement turn through the logs (chat id,
    // original and new request id, error code), never through message content
    let rig = rig_with_std(&json!({
        "context_window": 600, "max_input_tokens": 0, "estimation_budgets": byte_budgets(),
    }))
    .await;
    let chat = rig.std_chat().await;
    let content = format!("PRIVATE-WORDS {}", "z".repeat(140));
    let seeded = seed(&rig.quota.db, &chat, turn(&content)).await;
    let result = retry(&rig, &chat, &seeded).await;
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
    let replacement = replacement_of(&rig.quota.db, chat.id, seeded.request_id).await;
    assert!(logs_contain(&chat.id.to_string()), "the chat id is logged");
    assert!(
        logs_contain(&seeded.request_id.to_string()),
        "the original request id is logged"
    );
    assert!(
        logs_contain(&replacement.request_id.to_string()),
        "the new request id is logged"
    );
    assert!(
        logs_contain("context_length_exceeded"),
        "the error code is logged"
    );
    assert!(
        !logs_contain("PRIVATE-WORDS"),
        "message content is never logged"
    );
}
