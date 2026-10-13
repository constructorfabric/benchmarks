#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::too_many_lines,
    clippy::unused_async,
    clippy::let_underscore_must_use
)]
//! Slices 2 to 4, end to end: retry and edit through the router over the real
//! stores and the fake OAGW. The stream of the replacement with its new
//! server-generated `request_id`, the provider request (history and content),
//! the rows after `done`, the carry-forward, the summary, the refusals before
//! the commit that keep the previous turn, the setup failures after it, the
//! replaced request ids, the concurrent mutations on a file-backed WAL
//! database, a disconnect right after the commit, and the mutation metrics.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mini_chat_sdk::{KillSwitches, ModelTier, TurnAuditEventType, TurnMutationAuditEvent};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;

use crate::chat_test_env::Reply;
use crate::domain::service::mutations::types::{MutationOp, MutationResult};
use crate::domain::service::quota::test_support::{NO_SWITCHES, model};
use crate::mutation_test_env::{MutationEnv, SeedTurn, Seeded, TURN_TYPE};
use crate::stream_test_env::{Frame, OpenStream, Options, PREFIX, body, oa, parse_frames};
use crate::test_doubles::{USER, USER_TENANT, user_ctx};

// ---- helpers ------------------------------------------------------------------------------------

fn frames(reply: &Reply) -> Vec<Frame> {
    parse_frames(&reply.text)
}

fn names(reply: &Reply) -> Vec<String> {
    frames(reply).into_iter().map(|f| f.name).collect()
}

fn assert_stream(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply
            .header("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "{:?}",
        reply.headers
    );
}

fn assert_problem(reply: &Reply, status: StatusCode) {
    assert_eq!(reply.status, status, "{}", reply.text);
    assert!(
        reply
            .header("content-type")
            .unwrap_or_default()
            .contains("application/problem+json"),
        "a problem, not a stream: {:?}",
        reply.headers
    );
}

fn assert_conflict(reply: &Reply, reason: &str) {
    assert_problem(reply, StatusCode::CONFLICT);
    assert_eq!(reply.json["context"]["reason"], reason, "{}", reply.text);
}

fn assert_out_of_range(reply: &Reply, field: &str, reason: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(reply.category().contains("out_of_range"), "{}", reply.text);
    assert_eq!(
        reply.first_violation(),
        (field.to_owned(), reason.to_owned()),
        "{}",
        reply.text
    );
}

fn new_request(reply: &Reply) -> Uuid {
    MutationEnv::started_request_id(&frames(reply))
}

async fn sent_body(env: &MutationEnv) -> Value {
    env.oagw
        .proxied()
        .last()
        .expect("a provider request")
        .json()
}

fn input_texts(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["content"][0]["text"].as_str().unwrap().to_owned())
        .collect()
}

fn tools_of(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["type"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Two seeded completed turns; the second is the latest.
async fn two_turns(env: &MutationEnv) -> (Seeded, Seeded) {
    (
        env.seed_turn(&SeedTurn::completed(1, 10)).await,
        env.seed_turn(&SeedTurn::completed(2, 20)).await,
    )
}

async fn assert_refused_cleanly(env: &MutationEnv, before: &str, reply: &Reply) {
    assert_ne!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply
            .header("content-type")
            .unwrap_or_default()
            .contains("application/problem+json"),
        "a problem, not a stream"
    );
    assert_eq!(
        env.world().await,
        before,
        "the previous turn and everything else stay"
    );
    assert_eq!(env.oagw.proxy_call_count(), 0, "the provider heard nothing");
}

// ==== the stream of a replacement ====================================================================

#[tokio::test]
async fn a_retry_streams_with_a_new_server_generated_request_id_and_a_new_turn() {
    let env = MutationEnv::new().await;
    let (_, old) = two_turns(&env).await;
    env.script(&[
        oa::delta("Hello"),
        oa::delta(" again"),
        oa::completed(120, 34),
    ]);

    let reply = env.retry(old.request_id).await;

    assert_stream(&reply);
    assert_eq!(names(&reply), ["stream_started", "delta", "delta", "done"]);
    let started = &frames(&reply)[0].data;
    let new = Uuid::parse_str(started["request_id"].as_str().unwrap()).unwrap();
    assert_ne!(new, old.request_id);
    assert_eq!(new.get_version_num(), 4);
    assert_eq!(started["is_new_turn"], true);
    assert!(Uuid::parse_str(started["message_id"].as_str().unwrap()).is_ok());
    assert!(started.get("thread_summary_applied").is_none());
    let done = &frames(&reply)[3].data;
    assert_eq!(
        done["usage"],
        json!({ "input_tokens": 120, "output_tokens": 34 })
    );
    assert_eq!(done["effective_model"], "p");
    assert_eq!(done["selected_model"], "p");
    assert_eq!(done["quota_decision"], "allow");
    assert_eq!(env.turn_state_of(new).await, "completed");
}

#[tokio::test]
async fn the_response_headers_are_those_of_the_send_stream() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert_stream(&reply);
    assert_eq!(reply.header("cache-control"), Some("no-cache"));
    assert!(reply.header("content-encoding").is_none());
}

#[tokio::test]
async fn the_started_message_id_is_the_persisted_assistant_message_of_the_new_turn() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    let started = &frames(&reply)[0].data;
    let message = Uuid::parse_str(started["message_id"].as_str().unwrap()).unwrap();
    let new = new_request(&reply);
    assert_eq!(
        env.turn_col(new, "assistant_message_id").await,
        crate::stream_test_env::qblob(message)
    );
    let status = env.turn_status(new).await;
    assert_eq!(status.json["state"], "done");
    assert_eq!(status.json["assistant_message_id"], message.to_string());
}

// ==== what the provider receives =====================================================================

#[tokio::test]
async fn a_retry_sends_the_original_content_and_the_history_without_the_replaced_turn() {
    let env = MutationEnv::new().await;
    let (_, old) = two_turns(&env).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert_stream(&reply);
    assert_eq!(
        input_texts(&sent_body(&env).await),
        ["question 1", "answer 1", "question 2"],
        "history of the earlier turn, then the original content as the current message"
    );
}

#[tokio::test]
async fn an_edit_sends_the_edited_content_instead_of_the_original() {
    let env = MutationEnv::new().await;
    let (_, old) = two_turns(&env).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.edit(old.request_id, "  edited question \n").await;

    assert_stream(&reply);
    assert_eq!(
        input_texts(&sent_body(&env).await),
        ["question 1", "answer 1", "  edited question \n"],
        "sent exactly as given, not trimmed"
    );
    let new = new_request(&reply);
    let live = env.messages(env.chat).await;
    assert_eq!(live[2].1, "  edited question \n", "stored as sent");
    assert_eq!(
        env.messages_of(new).await[0].content,
        "  edited question \n"
    );
}

#[tokio::test]
async fn the_history_of_a_replaced_first_turn_is_empty() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    env.retry(old.request_id).await;

    assert_eq!(input_texts(&sent_body(&env).await), ["question 1"]);
}

#[tokio::test]
async fn the_provider_request_carries_the_model_of_the_chat_and_the_callers_context() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    env.retry(old.request_id).await;

    let sent = env.oagw.proxied();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].uri, "/api.openai.test/v1/responses");
    assert_eq!(sent[0].json()["model"], "provider-p");
    assert_eq!(sent[0].subject, USER);
    assert_eq!(sent[0].tenant, USER_TENANT);
    assert_eq!(sent[0].json()["metadata"]["chat_id"], env.chat.to_string());
}

// ==== the rows after done ============================================================================

#[tokio::test]
async fn after_done_the_rows_are_exactly_those_of_the_feature() {
    let env = MutationEnv::new().await;
    let (previous, old) = two_turns(&env).await;
    let updated_before = env.chat_updated_at().await;
    env.script(&[oa::delta("fresh answer"), oa::completed(120, 34)]);

    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);

    // The old turn: soft-deleted, replaced, lifecycle untouched.
    assert!(env.turn_deleted_at(old.request_id).await.is_some());
    assert_eq!(env.replaced_by_request(old.request_id).await, Some(new));
    assert_eq!(env.turn_state_of(old.request_id).await, "completed");
    assert!(
        env.messages_of(old.request_id)
            .await
            .iter()
            .all(|m| m.deleted)
    );
    // The previous turn is untouched.
    assert!(env.turn_deleted_at(previous.request_id).await.is_none());
    // The new turn: completed, all six preflight columns, owner, flag.
    assert_eq!(env.turn_state_of(new).await, "completed");
    for column in [
        "reserve_tokens",
        "max_output_tokens_applied",
        "reserved_credits_micro",
        "policy_version_applied",
        "effective_model",
        "minimal_generation_floor_applied",
    ] {
        assert!(
            env.turn_text(new, column).await.is_some(),
            "{column} is set"
        );
    }
    assert_eq!(
        env.turn_col(new, "requester_user_id").await,
        crate::stream_test_env::qblob(USER)
    );
    assert_eq!(
        env.turn_text(new, "web_search_enabled").await.as_deref(),
        Some("0")
    );
    // The new messages carry the new request id.
    let messages = env.messages_of(new).await;
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str(), m.deleted))
            .collect::<Vec<_>>(),
        [
            ("user", "question 2", false),
            ("assistant", "fresh answer", false)
        ]
    );
    // The visible conversation is the previous turn and the new one.
    assert_eq!(
        env.messages(env.chat)
            .await
            .iter()
            .map(|m| m.1.as_str())
            .collect::<Vec<_>>(),
        ["question 1", "answer 1", "question 2", "fresh answer"]
    );
    assert_ne!(
        env.chat_updated_at().await,
        updated_before,
        "chats.updated_at moved"
    );
    // Billing: one reserve and one settlement for the new turn only.
    let daily = env.quota.total("daily").await.expect("the bucket exists");
    assert_eq!(daily.reserved, 0, "settled");
    assert!(daily.spent > 0);
    assert_eq!(daily.calls, 1);
    assert_eq!(
        env.usage_events().await.len(),
        1,
        "one usage event: the new turn's"
    );
}

#[tokio::test]
async fn a_committed_retry_enqueues_turn_retry_and_the_finalization_adds_turn_completed() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);

    let mutation = env.mutation_events().await;
    assert_eq!(
        mutation,
        vec![TurnMutationAuditEvent::TurnRetry {
            tenant_id: USER_TENANT,
            actor_user_id: USER,
            chat_id: env.chat,
            original_request_id: old.request_id,
            new_request_id: new,
            timestamp: env.now(),
        }]
    );
    let turn_events = env.turn_audit_events().await;
    assert_eq!(turn_events.len(), 1);
    assert_eq!(turn_events[0].event_type, TurnAuditEventType::TurnCompleted);
    assert_eq!(turn_events[0].request_id, new);
}

#[tokio::test]
async fn an_edit_enqueues_turn_edit_and_no_payload_contains_content() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.edit(old.request_id, "a very private edit").await;
    let new = new_request(&reply);

    let events = env.mutation_events().await;
    assert!(matches!(
        &events[0],
        TurnMutationAuditEvent::TurnEdit { original_request_id, new_request_id, .. }
            if *original_request_id == old.request_id && *new_request_id == new
    ));
    let raw = serde_json::to_string(&env.audit_payloads().await).unwrap();
    assert!(!raw.contains("a very private edit") && !raw.contains("question 1"));
}

#[tokio::test]
async fn the_new_turn_is_reserved_before_the_provider_answers() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();

    let mut stream = env.open_retry(old.request_id).await;
    let started = stream.next_frame().await.unwrap();
    assert_eq!(started.name, "stream_started");
    let new = Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap();

    assert_eq!(env.turn_state_of(new).await, "running");
    assert!(
        env.turn_text(new, "reserve_tokens").await.is_some(),
        "the fill-mode reserve committed"
    );
    assert!(env.quota.total("daily").await.unwrap().reserved > 0);
    assert_eq!(
        env.messages(env.chat)
            .await
            .iter()
            .map(|m| m.1.as_str())
            .collect::<Vec<_>>(),
        ["question 1"],
        "only the new user message is visible while the answer is pending"
    );
    assert!(env.turn_deleted_at(old.request_id).await.is_some());
    drop(stream);
    drop(feed);
    env.wait_terminal(new).await;
}

#[tokio::test]
async fn the_turn_status_of_the_new_request_is_done_and_the_old_one_is_404() {
    let env = MutationEnv::new().await;
    let (_, old) = two_turns(&env).await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);

    assert_eq!(env.turn_status(new).await.json["state"], "done");
    env.turn_status(old.request_id)
        .await
        .assert_not_found(TURN_TYPE);
}

#[tokio::test]
async fn the_message_list_shows_only_the_new_turn_in_place_of_the_replaced_one() {
    let env = MutationEnv::new().await;
    let (_, old) = two_turns(&env).await;
    env.script(&[oa::delta("fresh"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);

    let list = env
        .get(&format!("/mini-chat/v1/chats/{}/messages", env.chat))
        .await;
    let rows: Vec<(String, String)> = list.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["request_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2], ("user".to_owned(), new.to_string()));
    assert_eq!(rows[3], ("assistant".to_owned(), new.to_string()));
    assert!(
        rows.iter()
            .all(|(_, request)| *request != old.request_id.to_string())
    );
    let detail = env.get(&format!("/mini-chat/v1/chats/{}", env.chat)).await;
    assert_eq!(detail.json["message_count"], 4);
}

#[tokio::test]
async fn a_failed_old_turn_is_retried_into_a_completed_replacement() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::failed(1, 10)).await;
    env.script(&[oa::delta("better"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert_stream(&reply);
    assert_eq!(env.turn_state_of(old.request_id).await, "failed");
    assert_eq!(env.turn_state_of(new_request(&reply)).await, "completed");
}

// ==== replaced request ids and idempotency ===========================================================

#[tokio::test]
async fn the_replaced_request_id_stays_taken_and_the_new_one_replays() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::delta("fresh"), oa::completed(7, 3)]);
    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);
    let calls = env.oagw.proxy_call_count();
    let quota = env.quota.dump("quota_usage").await;

    // The old request id is taken for ever.
    let resend = env
        .post(env.chat, &body("question 1", old.request_id))
        .await;
    assert_conflict(&resend, "request_id_conflict");
    env.turn_status(old.request_id)
        .await
        .assert_not_found(TURN_TYPE);
    assert_conflict(&env.retry(old.request_id).await, "NOT_LATEST_TURN");
    // The new request id replays the completed replacement without a provider call.
    let replayed = env.stream(env.chat, &body("anything", new)).await;
    assert_eq!(replayed.names(), ["stream_started", "delta", "done"]);
    assert_eq!(replayed.frames[0].data["is_new_turn"], false);
    assert_eq!(replayed.frames[1].data["content"], "fresh");
    assert_eq!(env.oagw.proxy_call_count(), calls);
    assert_eq!(
        env.quota.dump("quota_usage").await,
        quota,
        "no quota change"
    );
}

#[tokio::test]
async fn a_retry_of_the_replacement_creates_a_further_replacement() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::delta("one"), oa::completed(1, 1)]);
    let first = new_request(&env.retry(old.request_id).await);
    env.advance(1);
    env.script(&[oa::delta("two"), oa::completed(1, 1)]);

    let second_reply = env.retry(first).await;
    let second = new_request(&second_reply);

    assert_stream(&second_reply);
    assert_eq!(env.replaced_by_request(first).await, Some(second));
    assert!(env.turn_deleted_at(first).await.is_some());
    assert_eq!(env.live_turn_requests().await, vec![second]);
    assert_eq!(env.mutation_events().await.len(), 2);
    assert_eq!(input_texts(&sent_body(&env).await), ["question 1"]);
}

// ==== carry-forward ==================================================================================

#[tokio::test]
async fn the_original_attachments_web_search_and_images_are_carried_to_the_replacement() {
    let env = MutationEnv::new().await;
    let doc = env
        .seed_attachment(env.chat, 1, |row| row.set("filename", "report.pdf"))
        .await;
    let picture = env.seed_image(env.chat, 2).await;
    let gone = env.seed_attachment(env.chat, 3, |row| row).await;
    env.seed_vector_store(env.chat, "vs_chat").await;
    let old = env
        .seed_turn(
            &SeedTurn::completed(1, 10)
                .web_search(true)
                .attached(&[doc, picture, gone]),
        )
        .await;
    env.soft_delete_attachment(gone).await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert_stream(&reply);
    let new = new_request(&reply);
    // The new user message carries one link per non-deleted original attachment.
    let new_message = env.user_message_of(new).await;
    let mut carried = vec![doc, picture];
    carried.sort();
    assert_eq!(env.links_of(new_message).await, carried);
    let mut originals = vec![doc, picture, gone];
    originals.sort();
    assert_eq!(
        env.links_of(old.user_message).await,
        originals,
        "the old links stay"
    );
    // The provider request re-sends the image, with web search and file search.
    let sent = sent_body(&env).await;
    assert_eq!(
        sent["input"][0]["content"],
        json!([
            { "type": "input_text", "text": "question 1" },
            { "type": "input_image", "file_id": "file-prov2" }
        ])
    );
    assert_eq!(tools_of(&sent), ["file_search", "web_search"]);
    assert_eq!(
        env.turn_text(new, "web_search_enabled").await.as_deref(),
        Some("1")
    );
    // The message list shows the attachments on the new user message.
    let list = env
        .get(&format!("/mini-chat/v1/chats/{}/messages", env.chat))
        .await;
    let attached: Vec<String> = list.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["request_id"] == new.to_string() && m["role"] == "user")
        .unwrap()["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["attachment_id"].as_str().unwrap().to_owned())
        .collect();
    let mut expected: Vec<String> = carried.iter().map(Uuid::to_string).collect();
    expected.sort();
    let mut attached_sorted = attached;
    attached_sorted.sort();
    assert_eq!(
        attached_sorted, expected,
        "the deleted attachment is silently left out"
    );
}

#[tokio::test]
async fn a_turn_sent_without_web_search_is_retried_without_the_web_search_tool() {
    let env = MutationEnv::new().await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).web_search(false))
        .await;
    env.script(&[oa::completed(1, 1)]);

    env.retry(old.request_id).await;

    assert!(!tools_of(&sent_body(&env).await).contains(&"web_search".to_owned()));
}

#[tokio::test]
async fn an_edit_carries_the_attachments_too() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[doc]))
        .await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.edit(old.request_id, "edited").await;

    let new = new_request(&reply);
    assert_eq!(
        env.links_of(env.user_message_of(new).await).await,
        vec![doc]
    );
}

// ==== the summary ====================================================================================

#[tokio::test]
async fn a_summary_before_the_replaced_turn_is_kept_and_followed_by_the_replacement() {
    let env = MutationEnv::new().await;
    let previous = env
        .seed_turn(&SeedTurn::completed(1, 10).compressed())
        .await;
    let old = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    env.seed_summary_at(previous.assistant_message, 11).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert_eq!(
        frames(&reply)[0].data["thread_summary_applied"],
        json!({ "token_estimate": 42 })
    );
    let texts = input_texts(&sent_body(&env).await);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].ends_with("the summary text"));
    assert_eq!(texts[1], "question 2");
    assert_eq!(env.summary_count().await, 1);
}

#[tokio::test]
async fn a_summary_covering_the_replaced_turn_is_deleted_and_the_history_is_uncompressed() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10).compressed())
        .await;
    let old = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    env.seed_summary_at(old.assistant_message, 21).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;

    assert!(
        frames(&reply)[0]
            .data
            .get("thread_summary_applied")
            .is_none()
    );
    assert_eq!(
        input_texts(&sent_body(&env).await),
        ["question 1", "answer 1", "question 2"],
        "no summary item, the uncompressed history"
    );
    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

// ==== validation of the edit body ====================================================================

#[tokio::test]
async fn an_empty_or_whitespace_edit_is_empty_content_before_anything_else() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    for content in ["", "   ", "\n\t "] {
        let reply = env.edit(old.request_id, content).await;

        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{content:?}: {}",
            reply.text
        );
        reply.assert_invalid("content", "EMPTY_CONTENT");
        assert_refused_cleanly(&env, &before, &reply).await;
    }
    assert!(
        env.quota.pdp.requests().is_empty(),
        "no PDP call: a pure body check"
    );
}

#[tokio::test]
async fn an_empty_edit_wins_on_a_foreign_chat_and_before_the_pdp_decides() {
    let env = MutationEnv::with(Options {
        pdp: crate::test_doubles::PdpReply::Deny,
        ..Options::default()
    })
    .await;

    let reply = env
        .call(
            "PATCH",
            &MutationEnv::item_uri(Uuid::from_u128(0xDEAD), Uuid::from_u128(1)),
            &user_ctx(),
            Some(("application/json", json!({ "content": "  " }).to_string())),
        )
        .await;

    reply.assert_invalid("content", "EMPTY_CONTENT");
}

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_422_malformed_json_400_and_a_wrong_media_type_415()
 {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;
    let uri = MutationEnv::item_uri(env.chat, old.request_id);

    for bad in [
        json!({}),
        json!({ "content": 5 }),
        json!({ "content": null }),
    ] {
        let reply = env
            .call(
                "PATCH",
                &uri,
                &user_ctx(),
                Some(("application/json", bad.to_string())),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}: {}",
            reply.text
        );
        assert_eq!(reply.first_violation().1, "invalid_json_body", "{bad}");
        assert_refused_cleanly(&env, &before, &reply).await;
    }
    let malformed = env
        .call(
            "PATCH",
            &uri,
            &user_ctx(),
            Some(("application/json", "{not json".to_owned())),
        )
        .await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "{}",
        malformed.text
    );
    assert_eq!(malformed.first_violation().1, "json_syntax_error");
    let wrong_type = env
        .call(
            "PATCH",
            &uri,
            &user_ctx(),
            Some(("text/plain", "{\"content\":\"x\"}".to_owned())),
        )
        .await;
    assert_eq!(
        wrong_type.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        wrong_type.text
    );
    assert_eq!(wrong_type.first_violation().1, "missing_json_content_type");
    let no_type = env.call("PATCH", &uri, &user_ctx(), None).await;
    assert_eq!(
        no_type.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        no_type.text
    );
    assert_eq!(env.world().await, before);
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn unknown_body_fields_of_an_edit_are_ignored() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env
        .call(
            "PATCH",
            &MutationEnv::item_uri(env.chat, old.request_id),
            &user_ctx(),
            Some((
                "application/json",
                json!({ "content": "edited", "model": "s", "request_id": Uuid::new_v4() })
                    .to_string(),
            )),
        )
        .await;

    assert_stream(&reply);
    assert_eq!(input_texts(&sent_body(&env).await), ["edited"]);
    assert_ne!(new_request(&reply), old.request_id);
}

#[tokio::test]
async fn a_body_sent_with_a_retry_is_ignored() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    let reply = env
        .call(
            "POST",
            &MutationEnv::retry_uri(env.chat, old.request_id),
            &user_ctx(),
            Some((
                "application/json",
                json!({ "content": "ignored" }).to_string(),
            )),
        )
        .await;

    assert_stream(&reply);
    assert_eq!(input_texts(&sent_body(&env).await), ["question 1"]);
}

// ==== refusals before the commit keep the previous turn ===============================================

fn tight_catalog(max_input: u32) -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let mut tight = model("p", ModelTier::Premium)
        .text_budgets(4, 100, 10)
        .build();
    tight.max_input_tokens = max_input;
    vec![tight, model("s", ModelTier::Standard).build()]
}

#[tokio::test]
async fn a_chat_model_removed_from_the_catalog_is_invalid_model_and_the_turn_stays() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.exec("UPDATE chats SET model = 'gone'").await;
    let before = env.world().await;

    for reply in [
        env.retry(old.request_id).await,
        env.edit(old.request_id, "x").await,
    ] {
        reply.assert_invalid("model", "INVALID_MODEL");
        assert_refused_cleanly(&env, &before, &reply).await;
    }
}

#[tokio::test]
async fn an_exhausted_quota_is_429_and_the_turn_stays() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.quota
        .seed_bucket(&crate::quota_test_env::BucketSeed::total(
            "daily", 60_000_000, 0,
        ))
        .await;
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_problem(&reply, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.json["context"]["violations"][0]["subject"], "tokens");
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn web_search_under_its_kill_switch_is_refused_for_a_turn_sent_with_it() {
    let env = MutationEnv::with(Options {
        switches: KillSwitches {
            disable_web_search: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    let with_search = env
        .seed_turn(&SeedTurn::completed(1, 10).web_search(true))
        .await;
    let before = env.world().await;

    let reply = env.retry(with_search.request_id).await;

    assert_problem(&reply, StatusCode::BAD_REQUEST);
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(
        (violation["subject"].as_str(), violation["type"].as_str()),
        (Some("web_search"), Some("FEATURE_DISABLED"))
    );
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn images_under_their_kill_switch_are_refused_not_dropped() {
    let env = MutationEnv::with(Options {
        switches: KillSwitches {
            disable_images: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    let picture = env.seed_image(env.chat, 1).await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[picture]))
        .await;
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_problem(&reply, StatusCode::BAD_REQUEST);
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(
        (violation["subject"].as_str(), violation["type"].as_str()),
        (Some("images"), Some("FEATURE_DISABLED"))
    );
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_refused() {
    let env = MutationEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium).vision(false).build(),
            model("s", ModelTier::Standard).vision(false).build(),
        ],
        ..Options::default()
    })
    .await;
    let picture = env.seed_image(env.chat, 1).await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[picture]))
        .await;
    let before = env.world().await;

    let reply = env.edit(old.request_id, "look").await;

    reply.assert_invalid("attachment", "VISION_NOT_SUPPORTED");
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn too_many_carried_images_are_refused_before_any_write() {
    let env = MutationEnv::new().await;
    let mut pictures = Vec::new();
    for n in 1..=5 {
        pictures.push(env.seed_image(env.chat, n).await);
    }
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&pictures))
        .await;
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_out_of_range(&reply, "image_count", "TOO_MANY_IMAGES");
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn an_edit_above_max_input_tokens_is_input_too_long_and_a_short_one_passes() {
    let env = MutationEnv::with(Options {
        catalog: tight_catalog(1000),
        ..Options::default()
    })
    .await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    let long = env.edit(old.request_id, &"x".repeat(4000)).await;
    assert_out_of_range(&long, "content", "INPUT_TOO_LONG");
    assert_refused_cleanly(&env, &before, &long).await;
    env.script(&[oa::completed(1, 1)]);
    let short = env.edit(old.request_id, "short").await;
    assert_stream(&short);
}

#[tokio::test]
async fn a_retry_of_an_original_above_max_input_tokens_is_input_too_long() {
    let env = MutationEnv::with(Options {
        catalog: tight_catalog(1000),
        ..Options::default()
    })
    .await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).question(&"x".repeat(4000)))
        .await;
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_out_of_range(&reply, "content", "INPUT_TOO_LONG");
    assert_refused_cleanly(&env, &before, &reply).await;
}

#[tokio::test]
async fn a_policy_plugin_failure_is_a_500_and_the_turn_stays() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.quota
        .policy
        .fail(crate::domain::service::quota::test_support::PolicyStep::Snapshot);
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_problem(&reply, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !reply.text.contains("Snapshot"),
        "no internal detail: {}",
        reply.text
    );
    assert_refused_cleanly(&env, &before, &reply).await;
    assert_eq!(
        env.mutation_metrics.results(),
        vec![(MutationOp::Retry, MutationResult::Error)]
    );
}

#[tokio::test]
async fn a_failing_replacement_transaction_is_a_500_and_nothing_is_left_behind() {
    let healthy = MutationEnv::new().await;
    let fine = healthy.seed_turn(&SeedTurn::completed(1, 10)).await;
    healthy.script(&[oa::completed(1, 1)]);
    assert_stream(&healthy.retry(fine.request_id).await);
    let env = MutationEnv::with_failing_mutation_audit().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    let reply = env.retry(old.request_id).await;

    assert_problem(&reply, StatusCode::INTERNAL_SERVER_ERROR);
    assert_refused_cleanly(&env, &before, &reply).await;
    assert!(!reply.text.contains("not_registered"));
}

// ==== failures after the commit mark the new turn failed ==============================================

#[allow(clippy::cognitive_complexity)]
async fn assert_setup_failed(env: &MutationEnv, old: &Seeded, reply: &Reply, code: &str) -> Uuid {
    assert_problem(
        reply,
        if code == "context_length_exceeded" {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        },
    );
    // The previous turn is already replaced.
    assert!(env.turn_deleted_at(old.request_id).await.is_some());
    let live = env.live_turn_requests().await;
    assert_eq!(live.len(), 1, "the new turn is the only live turn");
    let new = live[0];
    assert_eq!(env.turn_state_of(new).await, "failed");
    assert_eq!(
        env.turn_text(new, "error_code").await.as_deref(),
        Some(code)
    );
    assert!(env.turn_text(new, "completed_at").await.is_some());
    assert_eq!(
        env.turn_text(new, "reserve_tokens").await,
        None,
        "no reserve exists"
    );
    // Nothing else: no settlement, no usage event, no finalization audit event, no answer.
    assert_eq!(env.usage_events().await.len(), 0);
    assert!(env.turn_audit_events().await.is_empty());
    let messages = env.messages_of(new).await;
    assert_eq!(
        messages.len(),
        1,
        "the new user message stays live, no assistant message"
    );
    assert!(!messages[0].deleted);
    assert_eq!(
        env.mutation_events().await.len(),
        1,
        "the committed turn_retry event is kept"
    );
    assert_eq!(env.oagw.proxy_call_count(), 0);
    // The turn status reports the code.
    let status = env.turn_status(new).await;
    assert_eq!(status.json["state"], "error");
    assert_eq!(status.json["error_code"], code);
    assert_eq!(
        env.mutation_metrics.results(),
        vec![(MutationOp::Retry, MutationResult::SetupFailed)]
    );
    new
}

#[tokio::test]
async fn a_context_budget_failure_is_400_with_the_turn_failed_context_length_exceeded() {
    let mut tiny = model("p", ModelTier::Premium).build();
    tiny.context_window = 400; // below the 500 output tokens that apply
    let env = MutationEnv::with(Options {
        catalog: vec![tiny, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let quota = env.quota.dump("quota_usage").await;

    let reply = env.retry(old.request_id).await;

    assert_out_of_range(&reply, "content", "CONTEXT_BUDGET_EXCEEDED");
    assert_setup_failed(&env, &old, &reply, "context_length_exceeded").await;
    assert_eq!(
        env.quota.dump("quota_usage").await,
        quota,
        "no quota_usage change"
    );
}

#[tokio::test]
async fn a_provider_resolution_failure_is_500_with_the_turn_failed_turn_setup_failed() {
    let env = MutationEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium)
                .provider("not-configured")
                .build(),
            model("s", ModelTier::Standard)
                .provider("not-configured")
                .build(),
        ],
        ..Options::default()
    })
    .await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    let reply = env.retry(old.request_id).await;

    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !reply.text.contains("not-configured"),
        "no provider detail: {}",
        reply.text
    );
    assert_setup_failed(&env, &old, &reply, "turn_setup_failed").await;
}

#[tokio::test]
async fn after_a_setup_failure_the_chat_is_not_blocked_and_the_failed_turn_can_be_retried() {
    let mut tiny = model("p", ModelTier::Premium).build();
    tiny.context_window = 400;
    let env = MutationEnv::with(Options {
        catalog: vec![tiny, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.retry(old.request_id).await;
    let failed = env.live_turn_requests().await[0];

    // A following send is accepted (it fails the same budget check, but is not
    // refused for a running turn), and the failed turn is the latest terminal
    // turn: it can be deleted.
    let send = env
        .post(env.chat, &body("next", Uuid::from_u128(0xE2FF_0009)))
        .await;
    assert_ne!(
        send.json["context"]["reason"], "turn_already_running",
        "{}",
        send.text
    );
    let deleted = env.delete(failed).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
}

// ==== concurrent mutations ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_retries_yield_one_stream_and_one_conflict() {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();

    let (a, b) = tokio::join!(
        env.open_retry(old.request_id),
        env.open_retry(old.request_id)
    );

    let (mut winner, loser) = if a.status == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(winner.status, StatusCode::OK);
    let loser = loser.into_reply().await;
    assert_problem(&loser, StatusCode::CONFLICT);
    let reason = loser.json["context"]["reason"].as_str().unwrap().to_owned();
    assert!(
        reason == "GENERATION_IN_PROGRESS" || reason == "NOT_LATEST_TURN",
        "never 500 and never a second stream: {reason}"
    );
    let started = winner.next_frame().await.unwrap();
    let new = Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap();
    // One replaced old turn, one new live turn, one new user message, one event, one reserve.
    assert_eq!(env.live_turn_requests().await, vec![new]);
    assert_eq!(env.replaced_by_request(old.request_id).await, Some(new));
    assert_eq!(env.messages_of(new).await.len(), 1);
    assert_eq!(env.mutation_events().await.len(), 1);
    assert_eq!(env.count("chat_turns").await, 2);
    let reserved = env.quota.total("daily").await.unwrap().reserved;
    assert!(reserved > 0);
    drop(winner);
    drop(feed);
    env.wait_terminal(new).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_retry_and_edit_yield_one_stream_and_one_conflict() {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();

    let (a, b) = tokio::join!(
        env.open_retry(old.request_id),
        env.open_edit(old.request_id, "edited")
    );

    let statuses = [a.status, b.status];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(env.live_turn_requests().await.len(), 1);
    assert_eq!(env.mutation_events().await.len(), 1);
    drop((a, b, feed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_deletes_yield_one_204_and_one_not_the_latest_turn() {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    let (a, b) = tokio::join!(env.delete(old.request_id), env.delete(old.request_id));

    let replies = [a, b];
    assert_eq!(
        replies
            .iter()
            .filter(|r| r.status == StatusCode::NO_CONTENT)
            .count(),
        1
    );
    let loser = replies
        .iter()
        .find(|r| r.status != StatusCode::NO_CONTENT)
        .unwrap();
    assert_conflict(loser, "NOT_LATEST_TURN");
    assert_eq!(env.mutation_events().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_retry_and_delete_yield_one_success_and_one_conflict() {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();

    let (retry, delete) = tokio::join!(env.open_retry(old.request_id), env.delete(old.request_id));

    let retried = retry.status == StatusCode::OK;
    let deleted = delete.status == StatusCode::NO_CONTENT;
    assert!(
        retried ^ deleted,
        "never both: retry {}, delete {}",
        retry.status,
        delete.status
    );
    if retried {
        assert_conflict(&delete, "NOT_LATEST_TURN");
    } else {
        assert_conflict(&retry.into_reply().await, "NOT_LATEST_TURN");
    }
    assert_eq!(env.mutation_events().await.len(), 1);
    drop(feed);
}

#[tokio::test]
async fn a_send_that_wins_the_window_before_the_replacement_makes_the_retry_generation_in_progress()
{
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();
    let held: Arc<Mutex<Option<OpenStream>>> = Arc::new(Mutex::new(None));
    {
        let (router, slot, chat) = (env.stream.router.clone(), Arc::clone(&held), env.chat);
        *env.hooked.before_replace.lock().unwrap() = Some(Arc::new(move || {
            let (router, slot) = (router.clone(), Arc::clone(&slot));
            Box::pin(async move {
                let mut request = Request::builder()
                    .method("POST")
                    .uri(format!("{PREFIX}/v1/chats/{chat}/messages:stream"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "content": "a send wins the race" }).to_string(),
                    ))
                    .unwrap();
                request.extensions_mut().insert(user_ctx());
                let mut open = OpenStream::from_response(router.oneshot(request).await.unwrap());
                open.until("stream_started").await;
                *slot.lock().unwrap() = Some(open);
            })
        }));
    }

    let reply = env.retry(old.request_id).await;

    assert_conflict(&reply, "GENERATION_IN_PROGRESS");
    assert!(
        env.turn_deleted_at(old.request_id).await.is_none(),
        "the previous turn stays"
    );
    assert!(env.mutation_events().await.is_empty());
    assert_eq!(
        env.mutation_metrics.results(),
        vec![(MutationOp::Retry, MutationResult::GenerationInProgress)]
    );
    drop(held.lock().unwrap().take());
    drop(feed);
}

// ==== a winner that commits inside the window between the preview and the transaction ===============

/// Installs a hook that runs `winner` once, right before the replacement
/// transaction of the request under test opens.
fn run_once_before_replace(
    env: &MutationEnv,
    winner: &'static str,
    slot: Arc<Mutex<Option<OpenStream>>>,
) {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (router, chat) = (env.router.clone(), env.chat);
    *env.hooked.before_replace.lock().unwrap() = Some(Arc::new(move || {
        let (router, slot, fired) = (router.clone(), Arc::clone(&slot), Arc::clone(&fired));
        Box::pin(async move {
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let request_id = Uuid::from_u128(0xE200_0001);
            let (method, uri) = match winner {
                "retry" => ("POST", MutationEnv::retry_uri(chat, request_id)),
                _ => ("DELETE", MutationEnv::item_uri(chat, request_id)),
            };
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(user_ctx());
            let mut open = OpenStream::from_response(router.oneshot(request).await.unwrap());
            if winner == "retry" {
                assert_eq!(open.status, StatusCode::OK, "the winner streams");
                open.until("stream_started").await;
            } else {
                assert_eq!(open.status, StatusCode::NO_CONTENT, "the winner deletes");
            }
            *slot.lock().unwrap() = Some(open);
        })
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mutation_that_meets_the_running_replacement_of_a_winner_in_its_transaction_is_generation_in_progress()
 {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();
    let held: Arc<Mutex<Option<OpenStream>>> = Arc::new(Mutex::new(None));
    run_once_before_replace(&env, "retry", Arc::clone(&held));

    let loser = env.edit(old.request_id, "the loser").await;

    assert_conflict(&loser, "GENERATION_IN_PROGRESS");
    assert_eq!(
        env.live_turn_requests().await.len(),
        1,
        "exactly one new live turn"
    );
    assert_eq!(
        env.mutation_events().await.len(),
        1,
        "the winner's event only"
    );
    assert_eq!(env.count("chat_turns").await, 2);
    assert_eq!(env.count("messages").await, 3, "no message of the loser");
    assert_eq!(
        env.mutation_metrics.results(),
        vec![
            (MutationOp::Retry, MutationResult::Ok),
            (MutationOp::Edit, MutationResult::GenerationInProgress),
        ]
    );
    drop(held.lock().unwrap().take());
    drop(feed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mutation_whose_winner_already_deleted_the_turn_is_not_the_latest_turn() {
    let env = MutationEnv::file().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let held: Arc<Mutex<Option<OpenStream>>> = Arc::new(Mutex::new(None));
    run_once_before_replace(&env, "delete", Arc::clone(&held));

    let loser = env.retry(old.request_id).await;

    assert_conflict(&loser, "NOT_LATEST_TURN");
    assert_eq!(env.count("chat_turns").await, 1, "no turn of the loser");
    assert_eq!(
        env.mutation_events().await.len(),
        1,
        "the winner's turn_delete only"
    );
    assert!(env.live_turn_requests().await.is_empty());
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

// ==== a disconnect right after the commit ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::cognitive_complexity)]
async fn a_handler_dropped_right_after_the_commit_leaves_the_new_turn_cancelled_never_running() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let committed = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
        *env.hooked.after_replace.lock().unwrap() = Some(Arc::new(move || {
            let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
            Box::pin(async move {
                committed.notify_one();
                proceed.notified().await;
            })
        }));
    }
    let mut request = Request::builder()
        .method("POST")
        .uri(MutationEnv::retry_uri(env.chat, old.request_id))
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(user_ctx());
    let router = env.router.clone();
    let mut handler = tokio::spawn(async move { router.oneshot(request).await });

    tokio::select! {
        () = committed.notified() => {}
        _ = &mut handler => panic!("the retry ended before the replacement committed"),
    }
    handler.abort(); // the handler is gone
    proceed.notify_one();

    let new = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(TurnMutationAuditEvent::TurnRetry { new_request_id, .. }) =
                env.mutation_events().await.first()
            {
                break *new_request_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the replacement committed");
    let state = env.wait_terminal(new).await;
    assert!(
        state == "cancelled" || state == "failed",
        "never running: {state}"
    );
    // A following send to the chat is accepted.
    env.script(&[oa::delta("fine"), oa::completed(1, 1)]);
    let send = env
        .post(env.chat, &body("next", Uuid::from_u128(0xE2FF_0042)))
        .await;
    assert_stream(&send);
}

// ==== metrics =======================================================================================

#[tokio::test]
async fn a_committed_retry_records_ok_and_one_latency_sample_each_request_one_result() {
    let env = MutationEnv::new().await;
    let (older, old) = two_turns(&env).await;
    env.script(&[oa::completed(1, 1)]);

    env.retry(older.request_id).await; // not the latest
    env.retry(old.request_id).await; // ok

    assert_eq!(
        env.mutation_metrics.results(),
        vec![
            (MutationOp::Retry, MutationResult::NotLatestTurn),
            (MutationOp::Retry, MutationResult::Ok),
        ]
    );
    let latencies = env.mutation_metrics.latencies();
    assert_eq!(latencies.len(), 1);
    assert_eq!(latencies[0].0, MutationOp::Retry);
}

#[tokio::test]
async fn an_edit_records_edit_as_its_op() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.script(&[oa::completed(1, 1)]);

    env.edit(old.request_id, "edited").await;
    env.edit(old.request_id, "again").await;

    assert_eq!(
        env.mutation_metrics.results(),
        vec![
            (MutationOp::Edit, MutationResult::Ok),
            (MutationOp::Edit, MutationResult::NotLatestTurn),
        ]
    );
}

#[tokio::test]
async fn a_running_target_and_a_refused_edit_record_their_results() {
    let env = MutationEnv::new().await;
    let running = env.seed_turn(&SeedTurn::running(1, 10)).await;

    env.retry(running.request_id).await;
    env.edit(running.request_id, " ").await;

    assert_eq!(
        env.mutation_metrics.results(),
        vec![
            (MutationOp::Retry, MutationResult::NotTerminal),
            (MutationOp::Edit, MutationResult::Rejected),
        ]
    );
    assert!(env.mutation_metrics.latencies().is_empty());
}

// ==== the mutation time ==============================================================================

#[tokio::test]
async fn a_replacement_sorts_after_a_turn_that_started_in_the_future_of_this_pods_clock() {
    let env = MutationEnv::new().await;
    let previous = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    // About 6 years after the fixture epoch: far past the clock of the fixture.
    let skewed = SeedTurn::completed(2, 200_000_000);
    let old = env.seed_turn(&skewed).await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);

    let reply = env.retry(old.request_id).await;
    let new = new_request(&reply);

    assert_stream(&reply);
    let old_start =
        time::OffsetDateTime::from_unix_timestamp(crate::chat_test_env::BASE_UNIX + 200_000_000)
            .unwrap();
    let expected = old_start + time::Duration::microseconds(1);
    assert_eq!(
        env.turn_text(new, "started_at").await,
        Some(
            expected
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        ),
        "the new turn starts 1 microsecond after the replaced one"
    );
    assert_eq!(
        env.live_turn_requests().await,
        vec![previous.request_id, new]
    );
    let live = env.messages(env.chat).await;
    assert_eq!(
        live.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
        ["question 1", "answer 1", "question 2", "x"],
        "the new user message sorts after the remaining history and the answer after it"
    );
}
