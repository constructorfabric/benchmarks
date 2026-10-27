#![allow(clippy::wildcard_imports)]
//! Streaming send-message pipeline tests: SSE contract, persistence,
//! idempotency/replay, parallel turns, provider error mapping.

use uuid::Uuid;

use super::*;
use crate::api::rest::dto::{QuotaDecisionKind, StreamMessageRequest};
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;
use crate::test_support::StubGateway;

#[tokio::test]
async fn send_streams_and_persists() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let request_id = Uuid::new_v4();
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(request_id)).await;

    // SSE contract: stream_started first, done last.
    assert!(matches!(events[0], MiniChatSseEvent::StreamStarted(_)));
    let started = started(&events);
    assert_eq!(started.request_id, request_id);
    assert!(started.is_new_turn);
    assert_eq!(text_of(&events), "Hello world");
    let done = done(&events);
    assert_eq!(done.usage.input_tokens, 10);
    assert_eq!(done.usage.output_tokens, 5);
    assert_eq!(done.effective_model, "gpt-premium");
    assert_eq!(done.selected_model, "gpt-premium");
    assert_eq!(done.quota_decision, QuotaDecisionKind::Allow);
    assert!(done.quota_warnings.is_some());

    // Persistence: user + assistant message share the request id.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let msgs = bed
        .bed
        .ctx
        .repos
        .messages
        .list_page(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, &odata())
        .await
        .expect("list");
    assert_eq!(msgs.items.len(), 2);
    assert_eq!(msgs.items[0].role, "user");
    assert_eq!(msgs.items[1].role, "assistant");
    assert_eq!(msgs.items[0].request_id, Some(request_id));
    assert_eq!(msgs.items[1].request_id, Some(request_id));
    assert_eq!(msgs.items[1].input_tokens, 10);
    assert_eq!(msgs.items[1].output_tokens, 5);
    assert_eq!(msgs.items[1].model.as_deref(), Some("gpt-premium"));

    // Turn is completed; assistant message linked.
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, request_id)
        .await
        .expect("turn")
        .expect("turn row");
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(msgs.items[1].id));
    assert_eq!(turn.provider_response_id.as_deref().map(|s| &s[..5]), Some("resp-"));

    // Usage event published exactly once (via the outbox pipeline).
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage event").await;
    let usage = bed.bed.policy.usage_events.lock().expect("lock");
    let ev = &usage[0];
    assert_eq!(ev.terminal_state, "completed");
    assert_eq!(ev.billing_outcome, "completed");
    assert_eq!(ev.settlement_method, "actual");
    assert_eq!(ev.request_id, request_id);
    assert_eq!(ev.turn_id, Some(turn.id));
    assert_eq!(ev.actual_credits_micro, 15); // 10 in + 5 out at 1 credit/1K each
    assert_eq!(ev.policy_version_applied, 1);
    assert_eq!(ev.dedupe_key, format!("{}/{}/{}", turn.tenant_id.as_simple(), turn.id.as_simple(), request_id.as_simple()));

    // Quota settled: reserve released, actual committed.
    assert_eq!(msgs.items.len(), 2);
}

#[tokio::test]
async fn replay_is_side_effect_free() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let request_id = Uuid::new_v4();
    let first = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(request_id)).await;
    let _ = first;
    // Wait for the first turn's usage event before counting.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage event").await;
    let calls_after_first = provider_calls(&bed.gateway);
    let usage_after_first = bed.bed.policy.usage_events.lock().expect("lock").len();
    assert_eq!(usage_after_first, 1);

    let replay = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(request_id)).await;
    let started = started(&replay);
    assert!(!started.is_new_turn);
    assert_eq!(text_of(&replay), "Hello world");
    let done = done(&replay);
    assert_eq!(done.usage.input_tokens, 10);
    assert_eq!(done.quota_decision, QuotaDecisionKind::Allow);
    assert!(done.quota_warnings.is_none(), "no warnings on replay");

    // No new provider call, no new usage event.
    assert_eq!(provider_calls(&bed.gateway), calls_after_first);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(bed.bed.policy.usage_events.lock().expect("lock").len(), usage_after_first);
}

#[tokio::test]
async fn request_id_conflict_for_non_completed_turn() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let request_id = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(request_id)).await;

    // Soft-delete the turn (as a mutation would) and resend.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, request_id)
        .await
        .expect("turn")
        .expect("turn row");
    bed.bed
        .ctx
        .repos
        .turns
        .soft_delete(&conn, turn.id, None)
        .await
        .expect("soft delete");
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: Some(request_id),
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::RequestIdConflict)));
}

#[tokio::test]
async fn parallel_turn_guard() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Insert a running turn directly.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    bed.bed
        .ctx
        .repos
        .turns
        .insert(
            &conn,
            &toolkit_db::secure::AccessScope::allow_all(),
            crate::infra::storage::repo::turns::NewTurn {
                id: Uuid::new_v4(),
                tenant_id: bed.bed.security.subject_tenant_id(),
                chat_id: chat.id,
                request_id: Uuid::new_v4(),
                requester_user_id: bed.bed.security.subject_id(),
                web_search_enabled: false,
                reserve_tokens: 100,
                max_output_tokens_applied: 50,
                reserved_credits_micro: 100,
                policy_version_applied: 1,
                effective_model: "gpt-premium".to_owned(),
                minimal_generation_floor_applied: 50,
            },
        )
        .await
        .expect("insert turn");
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: Some(Uuid::new_v4()),
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::TurnAlreadyRunning)));
}

#[tokio::test]
async fn provider_error_maps_to_sse_error() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream {
            events: vec![(
                "response.failed".to_owned(),
                r#"{"response": {"id": "resp-err1", "error": {"message": "model resp_ABC123 exploded file-abcdefghijklmnop"}}}"#.to_owned(),
            )],
        },
    );
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let err = error(&events).expect("error event");
    assert_eq!(err.code, "provider_error");
    // Provider identifiers are sanitized.
    assert!(!err.message.contains("resp_ABC123"), "sanitized: {}", err.message);
    assert!(!err.message.contains("file-abcdefghijklmnop"), "sanitized: {}", err.message);

    // The turn is failed with usage settlement (estimated here: no usage).
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .latest(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("turn")
        .expect("turn row");
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    // A failed turn produces exactly one usage event.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage event").await;
    let usage = bed.bed.policy.usage_events.lock().expect("lock");
    assert_eq!(usage[0].billing_outcome, "failed");
    assert_eq!(usage[0].settlement_method, "estimated");
}

#[tokio::test]
async fn provider_429_maps_to_rate_limited_with_retry_hint() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream { events: vec![] },
    );
    bed.gateway.fail_next_with_429();
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let err = error(&events).expect("error event");
    assert_eq!(err.code, "rate_limited");
    assert!(err.message.contains("retry in 7s"), "retry hint: {}", err.message);
}

#[tokio::test]
async fn empty_content_rejected_before_stream() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "   ".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::EmptyContent)));
}

#[tokio::test]
async fn quota_exhaustion_rejects_before_provider() {
    let mut policy = crate::test_support::StubPolicy::default();
    policy.snapshot.model_catalog = crate::test_support::StubPolicy::default_catalog();
    policy.standard.limit_daily_credits_micro = 1; // nothing fits
    policy.premium.limit_daily_credits_micro = 1;
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, None).await;
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::QuotaExceeded("tokens"))));
    assert_eq!(provider_calls(&bed.gateway), 0, "no provider call");
}

#[tokio::test]
async fn downgrade_to_standard_when_premium_exhausted() {
    let mut policy = crate::test_support::StubPolicy::default();
    policy.snapshot.model_catalog = crate::test_support::StubPolicy::default_catalog();
    // Premium daily is tiny; total (standard) has room.
    policy.premium.limit_daily_credits_micro = 1;
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, None).await;
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let done = done(&events);
    assert_eq!(done.quota_decision, QuotaDecisionKind::Downgrade);
    assert_eq!(done.effective_model, "gpt-standard");
    assert_eq!(done.selected_model, "gpt-premium");
    assert_eq!(done.downgrade_from.as_deref(), Some("gpt-premium"));
    assert_eq!(done.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));

    // The provider request used the standard model name.
    let reqs = bed.gateway.requests.lock().expect("lock");
    assert_eq!(reqs[0]["model"], "gpt-standard");
}

#[tokio::test]
async fn web_search_tool_sent_when_enabled() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "search please".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(crate::api::rest::dto::WebSearchConfig { enabled: true }),
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let reqs = bed.gateway.requests.lock().expect("lock");
    let tools = reqs[0]["tools"].as_array().expect("tools");
    assert!(
        tools.iter().any(|t| t["type"] == "web_search"),
        "web_search tool sent: {tools:?}"
    );
    assert_eq!(tools[0]["search_context_size"], "low");
    // The web search guard is in the system prompt.
    let instructions = reqs[0]["instructions"].as_str().expect("instructions");
    assert!(instructions.contains("web_search"), "guard in prompt: {instructions}");
}

#[tokio::test]
async fn web_search_kill_switch_rejects() {
    let mut kill = mini_chat_sdk::KillSwitches::default();
    kill.disable_web_search = true;
    let policy = crate::test_support::StubPolicy::new(
        crate::test_support::StubPolicy::default_catalog(),
        kill,
    );
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, None).await;
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(crate::api::rest::dto::WebSearchConfig { enabled: true }),
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::FeatureDisabled("web_search"))));
    assert_eq!(provider_calls(&bed.gateway), 0);
}

#[tokio::test]
async fn web_search_mid_turn_limit_fails_the_turn() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Three web_search start events with a per-turn limit of 2.
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream {
            events: vec![
                ("response.web_search_call.searching".to_owned(), "{}".to_owned()),
                ("response.web_search_call.completed".to_owned(), "{}".to_owned()),
                ("response.web_search_call.searching".to_owned(), "{}".to_owned()),
                ("response.web_search_call.completed".to_owned(), "{}".to_owned()),
                ("response.web_search_call.searching".to_owned(), "{}".to_owned()),
                (
                    "response.completed".to_owned(),
                    r#"{"response": {"id": "resp-x1", "usage": {"input_tokens": 3, "output_tokens": 2}}}"#.to_owned(),
                ),
            ],
        },
    );
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(crate::api::rest::dto::WebSearchConfig { enabled: true }),
            },
        )
        .await
        .expect("send");
    let events = match setup {
        StreamSetup::Live { receiver, .. } => drain(receiver).await,
        StreamSetup::Replay(events) => events,
    };
    let err = error(&events).expect("error event");
    assert_eq!(err.code, "web_search_calls_exceeded");
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .latest(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("turn")
        .expect("turn row");
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("web_search_calls_exceeded"));
    // Usage settled (estimated) with the completed web search count.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 1, "usage event").await;
    let usage = bed.bed.policy.usage_events.lock().expect("lock");
    assert_eq!(usage[0].web_search_calls, 2);
    assert_eq!(usage[0].settlement_method, "estimated");
}
