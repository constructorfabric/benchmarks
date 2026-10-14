//! The thread-summary handler in-process over `SQLite`, the real OAGW summary client and the mock
//! gateway, without network: the mapping of the domain outcome to the shared outbox result with the
//! 0-based `attempts`; a long conversation that enqueues the work, the handler that commits, and the
//! next stream that reports `thread_summary_applied` with the summary in its context; a failing
//! provider that retries and dead-letters on the third delivery with the previous summary intact; a
//! disabled summary model rejected at once; the summary call of each adapter kind with recorded
//! fixtures (request, S2S context, no credential, usage into the system-task event, prompt-too-long
//! retry, gateway Problem, hung call); the system-task isolation (no turn, no quota row, no audit
//! event, one usage event with the stable `system_request_id`); the operator replay and re-drive of
//! a dead-lettered work item; the absence of any gear-local worker table.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: flow
//! `cpt-cf-mini-chat-flow-background-maintenance-long-conversation` (`inst-lc-turn-completes`,
//! `inst-lc-deliver`, `inst-lc-handle`, `inst-lc-retry`, `inst-lc-redeliver`, `inst-lc-rejected`,
//! `inst-lc-dead-letter`, `inst-lc-next-turn`, `inst-lc-return`), flow
//! `cpt-cf-mini-chat-flow-background-maintenance-admin-observability` (`inst-adm-summary-alert`,
//! `inst-adm-summary-replay`, `inst-adm-summary-redrive`), algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring` (`inst-hw-build-summary`,
//! `inst-hw-outcomes`, `inst-hw-attempts`, `inst-hw-no-local-worker`, `inst-hw-stop`,
//! `inst-hw-return`), `cpt-cf-mini-chat-algo-background-maintenance-summary-handler` (`inst-sh-return`, `inst-sh-case-committed`), `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call` (`inst-call-build`,
//! `inst-call-identity`, `inst-call-post`), `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call` (`inst-ad-openai-responses`,
//! `inst-ad-chat-completions`, `inst-ad-vllm-responses`, `inst-ad-anthropic`, `inst-ad-post`,
//! `inst-ad-usage-match`), `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage` (`inst-su-build`, `inst-su-dedupe`,
//! `inst-su-stable-id`, `inst-su-enqueue`, `inst-su-no-turn`, `inst-su-no-audit`, `inst-su-return`)
//! and `cpt-cf-mini-chat-algo-background-maintenance-summary-commit` (`inst-cm-usage`, `inst-cm-return`); state
//! `cpt-cf-mini-chat-state-background-maintenance-summary-frontier` (`inst-fr-first-commit`,
//! `inst-fr-advance`, `inst-fr-unchanged`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`,
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-commit`,
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry`,
//! `cpt-cf-mini-chat-dod-background-maintenance-system-task-isolation` and
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use toolkit_db::outbox::LeasedMessageHandler;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::maintenance::settings::SummaryWorkerSettings;
use crate::infra::oagw::summary_client::OagwSummaryClient;
use crate::infra::provider_task::S2sContext;
use crate::infra::turn_finalizer::SummarySettings;
use crate::test_support::chats::{message_rows, outbox_rows};
use crate::test_support::maintenance::{
    MaintOptions, MaintRig, RecordingMaintenanceMetrics, SUMMARY_ANTHROPIC, SUMMARY_AZURE,
    SUMMARY_CC, SUMMARY_MODEL, SUMMARY_VLLM, SummaryWiring, answer_of, anthropic_answer,
    catalog_with, compressed_ids, context_length_error, maint_rig, maint_rig_with,
    responses_answer, result_label, result_reason, seed_range, settings_for, summary_entry,
    summary_message, summary_payload, summary_text, summary_worker_of, table_count,
};
use crate::test_support::mutations::summary_row;
use crate::test_support::provider::{GatewayFailure, MockProvider, Reply};
use crate::test_support::send::{
    SendRigOptions, parse_sse, rig_providers, seed_completed_turn, seed_summary, send_rig_with, t,
};
use crate::test_support::{TENANT_A, quota};

async fn seeded() -> (
    MaintRig,
    crate::infra::storage::repo_impl::ChatRecord,
    Vec<Uuid>,
) {
    let rig = maint_rig().await;
    let chat = rig.chat(quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 6).await;
    (rig, chat, ids)
}

fn payload_for(
    chat: &crate::infra::storage::repo_impl::ChatRecord,
    base: Option<(i64, Uuid)>,
    target: (i64, Uuid),
    request: Uuid,
) -> Value {
    summary_payload(
        chat.tenant_id,
        chat.id,
        base.map(|(micros, id)| (t(micros), id)),
        (t(target.0), target.1),
        request,
    )
}

fn good(rig: &MaintRig, kind: ProviderKind, text: &str, usage: (u64, u64)) {
    rig.upstream.set_default(Reply::http(
        200,
        &answer_of(kind, &summary_text(text), usage),
    ));
}

async fn usage_payloads(rig: &MaintRig) -> Vec<Value> {
    rig.usage_rows()
        .await
        .into_iter()
        .map(|row| row.payload)
        .collect()
}

// ── the outbox handler ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_handler_maps_ok_retry_and_reject_with_the_zero_based_attempts() {
    // inst-hw-outcomes, inst-hw-attempts, inst-sh-return: attempts 0, 1 and 2 of max_attempts 3
    let (rig, chat, ids) = seeded().await;
    rig.upstream
        .set_default(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, None, (50, ids[4]), Uuid::new_v4());
    let first = handler.handle(&summary_message(&payload, 0)).await;
    assert_eq!(result_label(&first), "retry");
    let second = handler.handle(&summary_message(&payload, 1)).await;
    assert_eq!(result_label(&second), "retry");
    let third = handler.handle(&summary_message(&payload, 2)).await;
    assert_eq!(result_label(&third), "reject");
    assert_eq!(
        result_reason(&third).as_deref(),
        Some("thread summary: max attempts (3) reached")
    );
    good(&rig, ProviderKind::OpenaiResponses, "S", (1, 1));
    let ok = handler.handle(&summary_message(&payload, 0)).await;
    assert_eq!(result_label(&ok), "ok");
}

#[tokio::test]
async fn a_malformed_message_is_rejected_by_the_handler() {
    // inst-hw-outcomes: a reason that never carries message content
    let (rig, _chat, _ids) = seeded().await;
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let message = crate::test_support::maintenance::message_of(
        "mini_chat.thread_summary",
        b"{\"tenant_id\": 7}",
        0,
    );
    let result = handler.handle(&message).await;
    assert_eq!(result_label(&result), "reject");
    assert_eq!(rig.upstream.call_count(), 0);
}

// ── a long conversation ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_long_conversation_enqueues_the_work_the_handler_commits_and_the_next_stream_applies_it()
{
    // inst-lc-turn-completes, inst-lc-deliver, inst-lc-handle, inst-lc-next-turn, inst-lc-return,
    // inst-fr-first-commit: the work is enqueued by the finalization of a turn whose history was
    // truncated, the handler summarizes exactly the frozen range, the next turn reports
    // `thread_summary_applied` and its context holds the summary and the messages after the
    // frontier
    let stream_upstream = MockProvider::new();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(stream_upstream.clone())
            .with_summary(SummarySettings {
                enabled: true,
                compression_threshold_pct: 1,
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut last_assistant = Uuid::nil();
    for i in 0..6i64 {
        let (_, assistant) = seed_completed_turn(
            &rig.quota.db,
            &chat,
            &format!("old question {i} {}", "x".repeat(1000)),
            &format!("old answer {i} {}", "y".repeat(1000)),
            t(10 + i * 20),
        )
        .await;
        last_assistant = assistant;
    }
    // the turn that causes the trigger: the 12 earlier messages reach 1 % of the input budget
    let reply = rig
        .stream(chat.id, &json!({ "content": "the current question" }))
        .await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text);
    rig.wait_idle(chat.id).await;
    let work = rig.summary_rows().await;
    assert_eq!(work.len(), 1, "one work item for the completed turn");
    assert_eq!(
        work[0].payload["frozen_target_message_id"],
        last_assistant.to_string()
    );
    assert!(work[0].payload["base_frontier_message_id"].is_null());

    // the handler commits over exactly the frozen range
    let summary_upstream = MockProvider::answering(Reply::http(
        200,
        &responses_answer(&summary_text("SUMMARY OF THE EARLIER TALK"), (500, 120)),
    ));
    let metrics = RecordingMaintenanceMetrics::new();
    let worker = summary_worker_of(SummaryWiring {
        store: rig.quota.db.provider.clone(),
        policy: rig.quota.gateway.clone(),
        providers: rig_providers(),
        transport: Arc::new(OagwSummaryClient::new(
            summary_upstream.clone(),
            S2sContext::bound(crate::test_support::send::provider_s2s()),
        )),
        usage: rig.quota.enqueuer.clone(),
        clock: rig.quota.clock.clone(),
        timer: crate::test_support::manual_time::ManualTime::auto(quota::default_now()),
        metrics: metrics.clone(),
        settings: settings_for(quota::STANDARD),
    });
    let turns_before = table_count(&rig.quota.db, "chat_turns").await;
    let quota_before = table_count(&rig.quota.db, "quota_usage").await;
    let outcome = worker
        .handle(&serde_json::to_vec(&work[0].payload).unwrap(), 0)
        .await;
    assert_eq!(outcome, crate::domain::maintenance::HandlerOutcome::Ok);
    assert_eq!(metrics.executions(), vec!["success"]);
    let row = summary_row(&rig.quota.db, chat.id)
        .await
        .expect("the summary is committed");
    assert_eq!(row.summary_text, "SUMMARY OF THE EARLIER TALK");
    assert_eq!(row.token_estimate, 120);
    assert_eq!(row.summarized_up_to_message_id, last_assistant);
    let compressed = compressed_ids(&rig.quota.db, chat.id).await;
    assert_eq!(
        compressed.len(),
        12,
        "exactly the frozen range: the 12 earlier messages"
    );
    // the causing turn's messages are not summarized
    let all = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(all.len(), 14);
    assert_eq!(all.iter().filter(|m| !m.is_compressed).count(), 2);
    // a system task: no turn, no quota row
    assert_eq!(table_count(&rig.quota.db, "chat_turns").await, turns_before);
    assert_eq!(
        table_count(&rig.quota.db, "quota_usage").await,
        quota_before
    );

    // the next stream
    let reply = rig
        .stream(chat.id, &json!({ "content": "the next question" }))
        .await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text);
    let frames = parse_sse(&reply.text);
    assert_eq!(
        frames[0].data["thread_summary_applied"],
        json!({ "token_estimate": 120 })
    );
    let input = stream_upstream.calls().last().unwrap().body["input"].to_string();
    assert!(
        input.contains("SUMMARY OF THE EARLIER TALK"),
        "the summary is in the context"
    );
    assert!(
        !input.contains("old question 0"),
        "compressed messages are not sent"
    );
    assert!(
        input.contains("the current question"),
        "the messages after the frontier are"
    );
    rig.wait_idle(chat.id).await;
}

// ── failure, retry and dead letter ───────────────────────────────────────────

#[tokio::test]
async fn a_failing_provider_retries_and_dead_letters_on_the_third_delivery_with_the_summary_intact()
{
    // inst-lc-retry, inst-lc-redeliver, inst-lc-rejected, inst-lc-dead-letter, inst-fr-unchanged
    let (rig, chat, ids) = seeded().await;
    seed_summary(rig.db(), &chat, "THE PREVIOUS SUMMARY", 9, (t(20), ids[1])).await;
    rig.upstream.set_default(Reply::http(
        503,
        &json!({ "error": { "message": "provider down" } }),
    ));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, Some((20, ids[1])), (60, ids[5]), Uuid::new_v4());
    let results: Vec<&str> = {
        let mut labels = Vec::new();
        for attempts in 0..3 {
            labels.push(result_label(
                &handler.handle(&summary_message(&payload, attempts)).await,
            ));
        }
        labels
    };
    assert_eq!(results, vec!["retry", "retry", "reject"]);
    assert_eq!(rig.upstream.call_count(), 3);
    let row = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(row.summary_text, "THE PREVIOUS SUMMARY");
    assert_eq!(
        row.summarized_up_to_message_id, ids[1],
        "the frontier did not move"
    );
    assert!(
        compressed_ids(rig.db(), chat.id).await.is_empty(),
        "no message is compressed"
    );
    assert!(
        rig.usage_rows().await.is_empty(),
        "no usage event for a failed call"
    );
    assert_eq!(rig.recorder.fallbacks(), 3);
    assert_eq!(rig.recorder.executions(), vec!["provider_error"; 3]);
}

#[tokio::test]
async fn a_disabled_summary_model_is_rejected_at_once_and_the_turns_keep_working() {
    // inst-sh-model-unavailable, inst-lc-rejected: `model_unavailable`, no retry
    let mut disabled =
        serde_json::to_value(summary_entry(SUMMARY_MODEL, "azure_openai", &json!({}))).unwrap();
    disabled["enabled"] = json!(false);
    let rig = maint_rig_with(
        MaintOptions::new().with_snapshot(catalog_with(serde_json::from_value(disabled).unwrap())),
    )
    .await;
    let chat = rig.chat(quota::STANDARD).await;
    let ids = seed_range(rig.db(), &chat, 4).await;
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let result = handler
        .handle(&summary_message(
            &payload_for(&chat, None, (40, ids[3]), Uuid::new_v4()),
            0,
        ))
        .await;
    assert_eq!(result_label(&result), "reject");
    assert_eq!(rig.recorder.executions(), vec!["model_unavailable"]);
    assert_eq!(rig.upstream.call_count(), 0);
    assert!(summary_row(rig.db(), chat.id).await.is_none());
}

#[tokio::test]
async fn an_empty_summary_retries_with_the_previous_summary_intact() {
    // inst-lc-retry: `empty_summary`, no fallback
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::http(
        200,
        &responses_answer("<analysis>nothing</analysis>", (10, 3)),
    ));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    let result = handler.handle(&summary_message(&payload, 0)).await;
    assert_eq!(result_label(&result), "retry");
    assert_eq!(rig.recorder.executions(), vec!["empty_summary"]);
    assert_eq!(rig.recorder.fallbacks(), 0);
    assert!(summary_row(rig.db(), chat.id).await.is_none());
}

#[tokio::test]
async fn a_hung_provider_ends_before_the_lease_and_the_message_is_retried() {
    // inst-call-post: the call ends when the time left of the provider phase is used up
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::Pending);
    let settings = SummaryWorkerSettings {
        claim_timeout: Duration::from_secs(30),
        ..settings_for(SUMMARY_MODEL)
    };
    let handler = rig.summary_handler(settings);
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    let result = handler.handle(&summary_message(&payload, 0)).await;
    assert_eq!(result_label(&result), "retry");
    assert_eq!(rig.time.elapsed(), Duration::from_secs(20));
    assert_eq!(rig.recorder.executions(), vec!["provider_error"]);
}

#[tokio::test]
async fn a_gateway_problem_is_a_provider_failure() {
    // inst-ad-post: the gateway's own 504 Problem is a failure, not an answer
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::gateway_problem(
        504,
        &json!({ "title": "Gateway Timeout" }),
    ));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "retry"
    );
    rig.upstream
        .set_default(Reply::Fail(GatewayFailure::Unavailable));
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 1)).await),
        "retry"
    );
    assert_eq!(rig.recorder.fallbacks(), 2);
}

// ── every adapter kind through the mock OAGW ─────────────────────────────────

async fn commit_with(
    model: &str,
    kind: ProviderKind,
    usage: (u64, u64),
) -> (MaintRig, Value, Value) {
    let (rig, chat, ids) = seeded().await;
    good(&rig, kind, "SUMMARY BODY", usage);
    let handler = rig.summary_handler(settings_for(model));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    let result = handler.handle(&summary_message(&payload, 0)).await;
    assert_eq!(result_label(&result), "ok", "{model}");
    let call = rig.upstream.only_call();
    let usage_event = usage_payloads(&rig).await.remove(0);
    let body = call.body.clone();
    assert_eq!(call.method, "POST");
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("accept"), Some("application/json"));
    for name in ["authorization", "api-key", "x-api-key"] {
        assert_eq!(call.header(name), None, "{model}: {name}");
    }
    assert_eq!(
        call.subject,
        crate::test_support::send::provider_s2s().subject_id()
    );
    assert_eq!(
        call.tenant,
        crate::test_support::send::provider_s2s().subject_tenant_id()
    );
    (rig, body, json!({ "uri": call.uri, "usage": usage_event }))
}

#[tokio::test]
async fn the_openai_responses_call_goes_through_the_gateway_and_its_usage_reaches_the_event() {
    // inst-ad-openai-responses, inst-ad-post, inst-call-build, inst-call-identity, inst-su-build
    let (rig, body, meta) =
        commit_with(SUMMARY_MODEL, ProviderKind::OpenaiResponses, (400, 90)).await;
    assert_eq!(meta["uri"], "/azure.example.com/v1/responses");
    assert_eq!(body["store"], false);
    assert_eq!(body["metadata"]["request_type"], "summary");
    assert_eq!(body["metadata"]["feature"], "none");
    assert_eq!(meta["usage"]["usage"]["input_tokens"], 400);
    assert_eq!(meta["usage"]["usage"]["output_tokens"], 90);
    assert_eq!(rig.recorder.executions(), vec!["success"]);
}

#[tokio::test]
async fn the_azure_responses_call_keeps_the_api_version_query() {
    // inst-ad-openai-responses: Azure differs only by the `api-version` query of `api_path`
    let (_rig, body, meta) =
        commit_with(SUMMARY_AZURE, ProviderKind::OpenaiResponses, (10, 5)).await;
    assert_eq!(
        meta["uri"],
        "/res.openai.azure.com/openai/deployments/provider-summ-azure-1/responses?api-version=2025-03-01-preview"
    );
    assert_eq!(body["model"], "provider-summ-azure-1");
}

#[tokio::test]
async fn the_chat_completions_call_sends_system_and_user_messages() {
    // inst-ad-chat-completions
    let (_rig, body, meta) =
        commit_with(SUMMARY_CC, ProviderKind::OpenaiChatCompletions, (30, 7)).await;
    assert_eq!(meta["uri"], "/cc.example.com/v1/chat/completions");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["stream"], false);
    assert!(body.get("stream_options").is_none() && body.get("tools").is_none());
    assert_eq!(meta["usage"]["usage"]["input_tokens"], 30);
}

#[tokio::test]
async fn the_vllm_call_has_no_metadata() {
    // inst-ad-vllm-responses
    let (_rig, body, meta) = commit_with(SUMMARY_VLLM, ProviderKind::VllmResponses, (20, 6)).await;
    assert_eq!(meta["uri"], "/vllm.example.com/v1/responses");
    assert!(body.get("metadata").is_none());
    assert_eq!(body["store"], false);
}

#[tokio::test]
async fn the_anthropic_call_reads_text_blocks_and_cache_write_usage() {
    // inst-ad-anthropic, inst-ad-usage-match
    let (rig, chat, ids) = seeded().await;
    let mut answer = anthropic_answer(&summary_text("ANTHROPIC SUMMARY"), (300, 70));
    answer["usage"]["cache_read_input_tokens"] = json!(200);
    answer["usage"]["cache_creation_input_tokens"] = json!(50);
    answer["content"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({ "type": "thinking", "thinking": "hmm" }));
    rig.upstream.set_default(Reply::http(200, &answer));
    let handler = rig.summary_handler(settings_for(SUMMARY_ANTHROPIC));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    let call = rig.upstream.only_call();
    assert_eq!(call.uri, "/anthropic.example.com/v1/messages");
    let cap = crate::test_support::maintenance::catalog()
        .model_catalog
        .into_iter()
        .find(|entry| entry.id == SUMMARY_ANTHROPIC)
        .unwrap()
        .max_output_tokens;
    assert_eq!(call.body["max_tokens"], cap);
    assert_eq!(call.body["metadata"]["user_id"].as_str().unwrap().len(), 64);
    let event = usage_payloads(&rig).await.remove(0);
    assert_eq!(event["usage"]["cache_read_input_tokens"], 200);
    assert_eq!(event["usage"]["cache_write_input_tokens"], 50);
    assert_eq!(
        summary_row(rig.db(), chat.id).await.unwrap().summary_text,
        "ANTHROPIC SUMMARY"
    );
}

#[tokio::test]
async fn a_vllm_think_block_never_reaches_the_stored_summary() {
    // inst-ad-vllm-responses: the `<think>` block is reasoning
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::http(
        200,
        &responses_answer(
            "<think>private reasoning</think><analysis>a</analysis><summary>VISIBLE</summary>",
            (5, 5),
        ),
    ));
    let handler = rig.summary_handler(settings_for(SUMMARY_VLLM));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    assert_eq!(
        summary_row(rig.db(), chat.id).await.unwrap().summary_text,
        "VISIBLE"
    );
}

#[tokio::test]
async fn a_context_length_rejection_is_retried_with_a_smaller_prompt_through_the_gateway() {
    // inst-call-post: at most two retries; the retried request carries fewer messages
    let (rig, chat, ids) = seeded().await;
    rig.upstream
        .queue(Reply::http(400, &context_length_error()));
    good(&rig, ProviderKind::OpenaiResponses, "S", (10, 4));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    let calls = rig.upstream.calls();
    assert_eq!(calls.len(), 2);
    let size = |call: &crate::test_support::provider::RecordedRequest| {
        call.body["input"][0]["content"].as_str().unwrap().len()
    };
    assert!(size(&calls[1]) < size(&calls[0]));
    // the whole frozen range is compressed even though the retry dropped its oldest message
    assert_eq!(compressed_ids(rig.db(), chat.id).await.len(), 6);
}

// ── system-task isolation ────────────────────────────────────────────────────

#[tokio::test]
async fn the_summary_is_a_system_task_with_one_usage_event_and_nothing_else() {
    // inst-su-no-turn, inst-su-no-audit, inst-su-enqueue, inst-cm-usage, inst-su-return
    let (rig, chat, ids) = seeded().await;
    good(&rig, ProviderKind::OpenaiResponses, "S", (100, 40));
    let outbox_before = outbox_rows(rig.db()).await.len();
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let request = Uuid::new_v4();
    let payload = payload_for(&chat, None, (60, ids[5]), request);
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    assert_eq!(table_count(rig.db(), "chat_turns").await, 0);
    assert_eq!(table_count(rig.db(), "quota_usage").await, 0);
    let rows = outbox_rows(rig.db()).await;
    assert_eq!(
        rows.len(),
        outbox_before + 1,
        "exactly one message was enqueued"
    );
    let audit = rows
        .iter()
        .filter(|r| r.queue == rig.quota.outbox.audit_queue_name)
        .count();
    assert_eq!(audit, 0, "no audit event");
    let event = usage_payloads(&rig).await.remove(0);
    assert_eq!(event["requester_type"], "system");
    assert_eq!(event["billing_outcome"], "system_task");
    assert_eq!(event["settlement_method"], "none");
    assert_eq!(event["actual_credits_micro"], 0);
    assert_eq!(event["request_id"], request.to_string());
    assert_eq!(event["effective_model"], SUMMARY_MODEL);
    assert_eq!(event["selected_model"], SUMMARY_MODEL);
    assert!(event.get("user_id").is_none() && event.get("turn_id").is_none());
    assert_eq!(
        event["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            TENANT_A.simple(),
            request.simple()
        )
    );
}

#[tokio::test]
async fn a_retry_after_a_failed_commit_reuses_the_system_request_id() {
    // inst-su-stable-id, inst-lc-redeliver: the handler never generates a new id
    let (rig, chat, ids) = seeded().await;
    good(&rig, ProviderKind::OpenaiResponses, "S", (1, 1));
    let failing = crate::test_support::maintenance::RecordingUsageQueue::failing();
    let request = Uuid::new_v4();
    let payload = serde_json::to_vec(&payload_for(&chat, None, (60, ids[5]), request)).unwrap();
    let failing_worker = summary_worker_of(SummaryWiring {
        store: rig.quota.db.provider.clone(),
        policy: rig.quota.gateway.clone(),
        providers: rig.providers.clone(),
        transport: Arc::new(OagwSummaryClient::new(
            rig.upstream.clone(),
            S2sContext::bound(crate::test_support::send::provider_s2s()),
        )),
        usage: failing.clone(),
        clock: rig.time.clone(),
        timer: rig.time.clone(),
        metrics: rig.recorder.clone(),
        settings: settings_for(SUMMARY_MODEL),
    });
    assert_eq!(
        failing_worker.handle(&payload, 0).await,
        crate::domain::maintenance::HandlerOutcome::Retry
    );
    assert_eq!(rig.recorder.executions(), vec!["retry"]);
    assert!(
        summary_row(rig.db(), chat.id).await.is_none(),
        "nothing of the attempt remains"
    );
    let again = rig.summary_worker(settings_for(SUMMARY_MODEL));
    assert_eq!(
        again.handle(&payload, 1).await,
        crate::domain::maintenance::HandlerOutcome::Ok
    );
    let attempted = failing.attempted();
    let committed = usage_payloads(&rig).await;
    assert_eq!(attempted[0].request_id, request);
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0]["request_id"], request.to_string());
    assert_eq!(
        attempted[0].dedupe_key,
        committed[0]["dedupe_key"].as_str().map(str::to_owned)
    );
}

// ── operator recovery ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_replayed_dead_letter_commits_only_while_the_frontier_still_equals_its_base() {
    // inst-adm-summary-replay: the replay re-runs the handler with the same `system_request_id`
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::http(500, &json!({})));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let request = Uuid::new_v4();
    let payload = payload_for(&chat, None, (40, ids[3]), request);
    for attempts in 0..3 {
        handler.handle(&summary_message(&payload, attempts)).await;
    }
    good(&rig, ProviderKind::OpenaiResponses, "AFTER THE FIX", (5, 5));
    // the cause is fixed: the replay commits
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    assert_eq!(
        summary_row(rig.db(), chat.id).await.unwrap().summary_text,
        "AFTER THE FIX"
    );
    // a second replay finds the frontier moved: no provider call, no second commit
    let calls = rig.upstream.call_count();
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    assert_eq!(rig.upstream.call_count(), calls);
    assert_eq!(rig.recorder.cas_conflicts(), 1);
    assert_eq!(usage_payloads(&rig).await.len(), 1);
}

#[tokio::test]
async fn a_replacement_work_item_with_a_wider_target_re_drives_a_stuck_frontier() {
    // inst-adm-summary-redrive: no new turn supersedes the range; the operator enqueues a wider
    // one for the same base
    let (rig, chat, ids) = seeded().await;
    rig.upstream.set_default(Reply::http(500, &json!({})));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let stuck = payload_for(&chat, None, (40, ids[3]), Uuid::new_v4());
    for attempts in 0..3 {
        handler.handle(&summary_message(&stuck, attempts)).await;
    }
    assert!(
        summary_row(rig.db(), chat.id).await.is_none(),
        "the frontier is stuck"
    );
    good(&rig, ProviderKind::OpenaiResponses, "WIDER", (5, 5));
    let wider = payload_for(&chat, None, (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&wider, 0)).await),
        "ok"
    );
    let row = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(row.summarized_up_to_message_id, ids[5]);
    assert_eq!(compressed_ids(rig.db(), chat.id).await.len(), 6);
}

#[tokio::test]
async fn the_second_summary_advances_the_frontier_with_the_first_as_its_base() {
    // inst-fr-advance: the merge prompt carries the stored summary
    let (rig, chat, ids) = seeded().await;
    good(&rig, ProviderKind::OpenaiResponses, "FIRST", (5, 5));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let first = payload_for(&chat, None, (40, ids[3]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&first, 0)).await),
        "ok"
    );
    good(&rig, ProviderKind::OpenaiResponses, "SECOND", (6, 6));
    let second = payload_for(&chat, Some((40, ids[3])), (60, ids[5]), Uuid::new_v4());
    assert_eq!(
        result_label(&handler.handle(&summary_message(&second, 0)).await),
        "ok"
    );
    let calls = rig.upstream.calls();
    let merge = calls[1].body["input"][0]["content"].as_str().unwrap();
    assert!(merge.contains("<existing_summary>") && merge.contains("FIRST"));
    assert!(
        merge.contains("m5") && !merge.contains("User: m1"),
        "only the new messages follow"
    );
    assert_eq!(
        summary_row(rig.db(), chat.id).await.unwrap().summary_text,
        "SECOND"
    );
    assert_eq!(usage_payloads(&rig).await.len(), 2);
}

// ── no gear-local worker ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_schema_has_no_claim_or_task_table_for_either_path() {
    // inst-hw-no-local-worker: delivery, ordering, leases, retries and dead letters are the shared
    // outbox's; no gear-local claim, reclaim or task table exists
    let rig = maint_rig().await;
    let names = crate::test_support::maintenance::table_names(rig.db()).await;
    assert!(names.iter().any(|n| n == "thread_summaries"), "{names:?}");
    for name in &names {
        let lower = name.to_lowercase();
        assert!(
            !(lower.contains("claim")
                || lower.contains("summary_task")
                || lower.contains("cleanup_task")
                || lower.contains("worker")
                || lower.contains("lease_")),
            "{name}"
        );
    }
}

#[tokio::test]
async fn the_handler_is_wired_for_the_summary_worker_without_a_leader_or_a_poller() {
    // inst-hw-build-summary, inst-hw-return, inst-hw-stop: building the handler spawns nothing and
    // holds nothing that must be stopped; an interrupted delivery re-runs idempotently
    let (rig, chat, ids) = seeded().await;
    good(&rig, ProviderKind::OpenaiResponses, "S", (1, 1));
    let handler = rig.summary_handler(settings_for(SUMMARY_MODEL));
    let payload = payload_for(&chat, None, (40, ids[3]), Uuid::new_v4());
    // the same message delivered twice (a lease expiry): one committed summary
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 0)).await),
        "ok"
    );
    assert_eq!(
        result_label(&handler.handle(&summary_message(&payload, 1)).await),
        "ok"
    );
    assert_eq!(usage_payloads(&rig).await.len(), 1);
}
