//! Turn mutation tests (retry, edit, delete of the latest turn) through the HTTP router.

use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::quota::{Bucket, Period};
use crate::test_support::app::{
    NO_KILL_SWITCHES, PREMIUM_LIMITS, STANDARD_LIMITS, SseFrame, TestApp, TestResponse, ctx,
};
use crate::test_support::catalog::test_catalog;
use crate::test_support::gateway::{Responder, SseScript};
use crate::test_support::stream::{
    AUDIT_QUEUE, CHATS, SeedAttachment, USAGE_QUEUE, answer, chat_row, compress_messages,
    create_chat, event_names, message_attachments_of, messages_of, provider_calls, quota_rows,
    script_provider, script_provider_sequence, seed_attachment, seed_spent, seed_thread_summary,
    set_turn_requester, soft_delete_attachment, stream_uri, thread_summary_of, turn_of, turns_of,
};

const TURN_RESOURCE: &str = "gts.cf.core.mini_chat.turn.v1~";

struct Caller {
    tenant: Uuid,
    user: Uuid,
    who: SecurityContext,
}

fn new_user() -> Caller {
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    Caller {
        tenant,
        user,
        who: ctx(tenant, user),
    }
}

fn turn_uri(chat: Uuid, request_id: impl std::fmt::Display) -> String {
    format!("{CHATS}/{chat}/turns/{request_id}")
}

fn retry_uri(chat: Uuid, request_id: Uuid) -> String {
    format!("{}/retry", turn_uri(chat, request_id))
}

#[allow(clippy::result_large_err)] // test helper; the rejection is inspected in place
async fn retry(
    app: &TestApp,
    u: &Caller,
    chat: Uuid,
    request_id: Uuid,
) -> Result<Vec<SseFrame>, TestResponse> {
    app.stream("POST", &retry_uri(chat, request_id), &u.who, json!({}))
        .await
}

#[allow(clippy::result_large_err)] // test helper; the rejection is inspected in place
async fn edit(
    app: &TestApp,
    u: &Caller,
    chat: Uuid,
    request_id: Uuid,
    content: &str,
) -> Result<Vec<SseFrame>, TestResponse> {
    app.stream(
        "PATCH",
        &turn_uri(chat, request_id),
        &u.who,
        json!({ "content": content }),
    )
    .await
}

async fn delete(app: &TestApp, u: &Caller, chat: Uuid, request_id: Uuid) -> TestResponse {
    app.call("DELETE", &turn_uri(chat, request_id), &u.who, None)
        .await
}

fn uuid_of(v: &Value) -> Uuid {
    v.as_str().expect("uuid string").parse().expect("uuid")
}

/// The `request_id` of `stream_started`; the stream must have ended with `done`.
fn completed_request(frames: &[SseFrame]) -> Uuid {
    assert_eq!(frames[0].event, "stream_started", "{frames:?}");
    assert_eq!(frames.last().map(|f| f.event.as_str()), Some("done"));
    uuid_of(&frames[0].data["request_id"])
}

/// Sends a message and waits for its answer; returns the turn's `request_id`.
async fn send_turn(app: &TestApp, u: &Caller, chat: Uuid, body: Value) -> Uuid {
    match app.stream("POST", &stream_uri(chat), &u.who, body).await {
        Ok(frames) => completed_request(&frames),
        Err(res) => panic!("send rejected: {} {}", res.status, res.json),
    }
}

fn accepted(result: Result<Vec<SseFrame>, TestResponse>) -> Vec<SseFrame> {
    result.unwrap_or_else(|res| panic!("mutation rejected: {} {}", res.status, res.json))
}

#[track_caller]
fn rejected(result: Result<Vec<SseFrame>, TestResponse>) -> TestResponse {
    match result {
        Ok(frames) => panic!("mutation streamed: {frames:?}"),
        Err(res) => res,
    }
}

#[track_caller]
fn assert_aborted(res: &TestResponse, reason: &str) {
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(res.json["context"]["reason"], reason, "{}", res.json);
}

/// `(role, content)` of the chat's messages as the message list returns them.
async fn listed(app: &TestApp, u: &Caller, chat: Uuid) -> Vec<(String, String)> {
    let res = app
        .call("GET", &format!("{CHATS}/{chat}/messages"), &u.who, None)
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    res.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["content"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(r, c)| ((*r).to_owned(), (*c).to_owned()))
        .collect()
}

async fn message_count(app: &TestApp, u: &Caller, chat: Uuid) -> i64 {
    let res = app
        .call("GET", &format!("{CHATS}/{chat}"), &u.who, None)
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    res.json["message_count"].as_i64().expect("message_count")
}

async fn wait_for_payloads(app: &TestApp, queue: &str, n: usize) {
    TestApp::wait_until(&format!("{n} payload(s) on {queue}"), || async {
        app.outbox_payloads(queue).len() >= n
    })
    .await;
}

fn audit_of_type(app: &TestApp, event_type: &str) -> Vec<Value> {
    app.outbox_payloads(AUDIT_QUEUE)
        .into_iter()
        .filter(|a| a["event_type"] == event_type)
        .collect()
}

#[tokio::test]
async fn retry_replaces_latest_turn() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            answer(&["first answer"], 3, 2),
            answer(&["second answer"], 3, 2),
            answer(&["retried ", "answer"], 4, 3),
        ],
    );
    send_turn(&app, &u, chat, json!({"content": "first question"})).await;
    let old = send_turn(&app, &u, chat, json!({"content": "second question"})).await;
    let before = chat_row(&app, chat).await.updated_at;

    let frames = accepted(retry(&app, &u, chat, old).await);

    assert_eq!(
        event_names(&frames),
        ["stream_started", "delta", "delta", "done"]
    );
    let new = completed_request(&frames);
    assert_ne!(new, old);
    assert_eq!(new.get_version_num(), 4);
    assert_eq!(frames[0].data["is_new_turn"], true);

    let old_turn = turn_of(&app, chat, old).await;
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    assert_eq!(old_turn.state, "completed");
    let new_turn = turn_of(&app, chat, new).await;
    assert_eq!(new_turn.state, "completed");
    assert!(new_turn.deleted_at.is_none());
    assert_eq!(new_turn.requester_user_id, Some(u.user));
    assert_eq!(new_turn.effective_model.as_deref(), Some("gpt-premium"));
    assert!(new_turn.reserve_tokens.is_some_and(|t| t > 0));
    assert!(new_turn.policy_version_applied.is_some());
    assert!(
        new_turn
            .reserved_credits_micro
            .is_some_and(|credits| credits > 0)
    );

    assert_eq!(
        listed(&app, &u, chat).await,
        pairs(&[
            ("user", "first question"),
            ("assistant", "first answer"),
            ("user", "second question"),
            ("assistant", "retried answer"),
        ])
    );
    let live: Vec<_> = messages_of(&app, chat)
        .await
        .into_iter()
        .filter(|m| m.deleted_at.is_none())
        .collect();
    assert_eq!(live.len(), 4);
    assert!(live[2..].iter().all(|m| m.request_id == Some(new)));
    let old_messages: Vec<_> = messages_of(&app, chat)
        .await
        .into_iter()
        .filter(|m| m.request_id == Some(old))
        .collect();
    assert_eq!(old_messages.len(), 2);
    assert!(old_messages.iter().all(|m| m.deleted_at.is_some()));

    let status = app.call("GET", &turn_uri(chat, old), &u.who, None).await;
    assert_eq!(status.status, 404, "{}", status.json);
    assert_eq!(status.json["context"]["resource_type"], TURN_RESOURCE);
    assert!(chat_row(&app, chat).await.updated_at > before);

    // Two finalizations, the retry, the retried turn's finalization.
    wait_for_payloads(&app, AUDIT_QUEUE, 4).await;
    let mutation = audit_of_type(&app, "turn_retry");
    assert_eq!(mutation.len(), 1, "{mutation:?}");
    let mutation = &mutation[0];
    assert_eq!(mutation["kind"], "mutation");
    assert_eq!(mutation["original_request_id"], json!(old));
    assert_eq!(mutation["new_request_id"], json!(new));
    assert!(mutation["request_id"].is_null(), "{mutation}");
    assert_eq!(mutation["actor_user_id"], json!(u.user));
    assert_eq!(mutation["tenant_id"], json!(u.tenant));
    assert_eq!(mutation["chat_id"], json!(chat));

    let calls = provider_calls(&app);
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2]["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "first question"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "first answer"}]},
            {"role": "user", "content": [{"type": "input_text", "text": "second question"}]},
        ])
    );
}

#[tokio::test]
async fn edit_uses_new_content_and_keeps_attachments() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let image = seed_attachment(&app, SeedAttachment::image(u.tenant, chat, u.user)).await;
    let document = seed_attachment(&app, SeedAttachment::document(u.tenant, chat, u.user)).await;
    let removed = seed_attachment(&app, SeedAttachment::document(u.tenant, chat, u.user)).await;
    script_provider_sequence(
        &app,
        vec![answer(&["old answer"], 3, 2), answer(&["new answer"], 3, 2)],
    );
    let old = send_turn(
        &app,
        &u,
        chat,
        json!({"content": "old text", "attachment_ids": [image, document, removed]}),
    )
    .await;
    soft_delete_attachment(&app, removed).await;

    let new = completed_request(&accepted(edit(&app, &u, chat, old, "new text").await));

    let messages = messages_of(&app, chat).await;
    let user_message = |request_id: Uuid| {
        messages
            .iter()
            .find(|m| m.request_id == Some(request_id) && m.role == "user")
            .unwrap_or_else(|| panic!("user message of {request_id}"))
    };
    let (old_user, new_user) = (user_message(old), user_message(new));
    assert_eq!(new_user.content, "new text");
    assert!(new_user.deleted_at.is_none());
    assert_eq!(old_user.content, "old text");
    assert!(old_user.deleted_at.is_some());
    assert_eq!(
        turn_of(&app, chat, old).await.replaced_by_request_id,
        Some(new)
    );

    let links = message_attachments_of(&app, chat).await;
    let linked = |message_id: Uuid| {
        let mut ids: Vec<Uuid> = links
            .iter()
            .filter(|l| l.message_id == message_id)
            .map(|l| l.attachment_id)
            .collect();
        ids.sort();
        ids
    };
    let mut expected = vec![image, document];
    expected.sort();
    assert_eq!(linked(new_user.id), expected);
    let mut original = vec![image, document, removed];
    original.sort();
    assert_eq!(linked(old_user.id), original, "old links stay for audit");

    let calls = provider_calls(&app);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1]["input"],
        json!([{"role": "user", "content": [
            {"type": "input_text", "text": "new text"},
            {"type": "input_image", "file_id": format!("file-{}", image.simple())},
        ]}])
    );

    wait_for_payloads(&app, AUDIT_QUEUE, 3).await;
    let mutation = audit_of_type(&app, "turn_edit");
    assert_eq!(mutation.len(), 1, "{mutation:?}");
    assert_eq!(mutation[0]["original_request_id"], json!(old));
    assert_eq!(mutation[0]["new_request_id"], json!(new));
}

#[tokio::test]
async fn delete_removes_latest_turn() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(&app, vec![answer(&["one"], 3, 2), answer(&["two"], 3, 2)]);
    let first = send_turn(&app, &u, chat, json!({"content": "q1"})).await;
    let second = send_turn(&app, &u, chat, json!({"content": "q2"})).await;
    assert_eq!(message_count(&app, &u, chat).await, 4);
    let before = chat_row(&app, chat).await.updated_at;

    let res = delete(&app, &u, chat, second).await;

    assert_eq!(res.status, 204, "{}", res.json);
    assert!(res.json.is_null(), "{}", res.json);
    assert_eq!(
        listed(&app, &u, chat).await,
        pairs(&[("user", "q1"), ("assistant", "one")])
    );
    assert_eq!(message_count(&app, &u, chat).await, 2);
    let deleted = turn_of(&app, chat, second).await;
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.replaced_by_request_id, None);
    assert_eq!(turns_of(&app, chat).await.len(), 2, "no new turn");
    assert_eq!(provider_calls(&app).len(), 2, "no provider call");
    assert!(chat_row(&app, chat).await.updated_at > before);

    wait_for_payloads(&app, AUDIT_QUEUE, 3).await;
    let audit = audit_of_type(&app, "turn_delete");
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["kind"], "mutation");
    assert_eq!(audit[0]["request_id"], json!(second));
    assert!(audit[0]["original_request_id"].is_null(), "{}", audit[0]);
    assert!(audit[0]["new_request_id"].is_null(), "{}", audit[0]);
    assert_eq!(audit[0]["actor_user_id"], json!(u.user));
    assert_eq!(audit[0]["chat_id"], json!(chat));

    // The previous turn is the latest now.
    let res = delete(&app, &u, chat, first).await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert!(listed(&app, &u, chat).await.is_empty());
    assert_eq!(message_count(&app, &u, chat).await, 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario over every guard
async fn mutation_guards() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 1, 1));
    let first = send_turn(&app, &u, chat, json!({"content": "a"})).await;
    let second = send_turn(&app, &u, chat, json!({"content": "b"})).await;

    // Not the latest turn.
    assert_aborted(
        &rejected(retry(&app, &u, chat, first).await),
        "NOT_LATEST_TURN",
    );
    assert_aborted(
        &rejected(edit(&app, &u, chat, first, "x").await),
        "NOT_LATEST_TURN",
    );
    assert_aborted(&delete(&app, &u, chat, first).await, "NOT_LATEST_TURN");

    // Unknown request id.
    let unknown = delete(&app, &u, chat, Uuid::new_v4()).await;
    assert_eq!(unknown.status, 404, "{}", unknown.json);
    assert_eq!(unknown.json["context"]["resource_type"], TURN_RESOURCE);
    let unknown = rejected(retry(&app, &u, chat, Uuid::new_v4()).await);
    assert_eq!(unknown.status, 404, "{}", unknown.json);
    assert_eq!(unknown.json["context"]["resource_type"], TURN_RESOURCE);

    // Empty edit content.
    let empty = rejected(edit(&app, &u, chat, second, " \n ").await);
    assert_eq!(empty.status, 400, "{}", empty.json);
    assert_eq!(
        empty.json["context"]["field_violations"][0]["reason"], "EMPTY_CONTENT",
        "{}",
        empty.json
    );

    // Another user's chat.
    let other = new_user();
    assert_eq!(delete(&app, &other, chat, second).await.status, 404);
    assert_eq!(
        rejected(retry(&app, &other, chat, second).await).status,
        404
    );
    assert_eq!(
        rejected(edit(&app, &other, chat, second, "x").await).status,
        404
    );

    // A turn requested by someone else.
    let second_turn = turn_of(&app, chat, second).await;
    set_turn_requester(&app, second_turn.id, Uuid::new_v4()).await;
    let foreign = delete(&app, &u, chat, second).await;
    assert_eq!(foreign.status, 403, "{}", foreign.json);
    set_turn_requester(&app, second_turn.id, u.user).await;

    // Already deleted.
    assert_eq!(delete(&app, &u, chat, second).await.status, 204);
    assert_aborted(&delete(&app, &u, chat, second).await, "NOT_LATEST_TURN");
    assert_aborted(
        &rejected(retry(&app, &u, chat, second).await),
        "NOT_LATEST_TURN",
    );

    // Running target (checked before the latest-turn check), and a running newer turn.
    app.gateway.clear_rules();
    script_provider(&app, Responder::Sse(vec![SseScript::Hang]));
    let mut reader = app
        .open_stream("POST", &stream_uri(chat), &u.who, json!({"content": "c"}))
        .await
        .unwrap_or_else(|r| panic!("{}", r.json));
    let (started, _) = reader.next_frame().await.expect("stream_started");
    let running = uuid_of(&started.data["request_id"]);
    for res in [
        rejected(retry(&app, &u, chat, running).await),
        rejected(edit(&app, &u, chat, running, "x").await),
        delete(&app, &u, chat, running).await,
    ] {
        assert_eq!(res.status, 400, "{}", res.json);
        let v = &res.json["context"]["violations"][0];
        assert_eq!(v["subject"], "turn_state", "{}", res.json);
        assert_eq!(v["type"], "STATE", "{}", res.json);
    }
    assert_aborted(&delete(&app, &u, chat, first).await, "NOT_LATEST_TURN");
    drop(reader);

    let first_turn = turn_of(&app, chat, first).await;
    assert!(first_turn.deleted_at.is_none());
    assert_eq!(turn_of(&app, chat, running).await.deleted_at, None);
}

#[tokio::test]
async fn quota_rejection_leaves_previous_turn_intact() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, Some("gpt-premium")).await;
    script_provider(&app, answer(&["ok"], 1, 1));
    let old = send_turn(&app, &u, chat, json!({"content": "hi"})).await;
    for (bucket, spent) in [
        (Bucket::Premium, PREMIUM_LIMITS.limit_daily_credits_micro),
        (Bucket::Total, STANDARD_LIMITS.limit_daily_credits_micro),
    ] {
        seed_spent(&app, u.tenant, u.user, Period::Daily, bucket, spent).await;
    }

    for res in [
        rejected(retry(&app, &u, chat, old).await),
        rejected(edit(&app, &u, chat, old, "other").await),
    ] {
        assert_eq!(res.status, 429, "{}", res.json);
        assert_eq!(
            res.json["context"]["violations"][0]["subject"], "tokens",
            "{}",
            res.json
        );
    }

    let turns = turns_of(&app, chat).await;
    assert_eq!(turns.len(), 1);
    assert!(turns[0].deleted_at.is_none());
    assert_eq!(turns[0].replaced_by_request_id, None);
    assert!(
        messages_of(&app, chat)
            .await
            .iter()
            .all(|m| m.deleted_at.is_none())
    );
    assert_eq!(provider_calls(&app).len(), 1);
    // Still the latest turn: it can be deleted.
    assert_eq!(delete(&app, &u, chat, old).await.status, 204);
}

#[tokio::test]
async fn concurrent_mutations_one_wins() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, Responder::Sse(vec![SseScript::Hang]));
    script_provider_sequence(&app, vec![answer(&["ok"], 1, 1)]);
    let old = send_turn(&app, &u, chat, json!({"content": "hi"})).await;
    let uri = retry_uri(chat, old);

    let (first, second) = tokio::join!(
        app.open_stream("POST", &uri, &u.who, json!({})),
        app.open_stream("POST", &uri, &u.who, json!({})),
    );
    let (winner, loser) = match (first, second) {
        (Ok(opened), Err(rejected)) | (Err(rejected), Ok(opened)) => (opened, rejected),
        (Ok(_), Ok(_)) => panic!("both retries opened a stream"),
        (Err(one), Err(other)) => panic!("both retries rejected: {} / {}", one.json, other.json),
    };
    assert_eq!(loser.status, 409, "{}", loser.json);
    let reason = loser.json["context"]["reason"].as_str().unwrap_or_default();
    assert!(
        ["GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"].contains(&reason),
        "{}",
        loser.json
    );

    let turns = turns_of(&app, chat).await;
    assert_eq!(turns.len(), 2, "{turns:?}");
    let live: Vec<_> = turns.iter().filter(|t| t.deleted_at.is_none()).collect();
    assert_eq!(live.len(), 1, "{turns:?}");
    assert_ne!(live[0].request_id, old);
    assert_eq!(live[0].state, "running");
    let new_id = live[0].request_id;

    drop(winner);
    TestApp::wait_until("the retried turn is cancelled", || async {
        turn_of(&app, chat, new_id).await.state == "cancelled"
    })
    .await;
}

#[tokio::test]
async fn post_commit_setup_failure_marks_turn_failed() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 1, 1));
    let old = send_turn(&app, &u, chat, json!({"content": "x".repeat(400)})).await;
    // The original message (100 tokens) no longer fits: 150 - 100 fixed overhead = 50.
    let mut tiny = test_catalog();
    tiny[0].context_window = tiny[0].max_output_tokens + 150;
    app.usage.set_catalog(tiny);

    let res = rejected(retry(&app, &u, chat, old).await);

    assert_eq!(res.status, 400, "{}", res.json);
    assert_eq!(
        res.json["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED",
        "{}",
        res.json
    );
    let old_turn = turn_of(&app, chat, old).await;
    assert!(old_turn.deleted_at.is_some(), "the mutation committed");
    let new_id = old_turn.replaced_by_request_id.expect("replaced");
    let new_turn = turn_of(&app, chat, new_id).await;
    assert_eq!(new_turn.state, "failed");
    assert_eq!(
        new_turn.error_code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(new_turn.completed_at.is_some());
    assert_eq!(new_turn.reserve_tokens, None, "no reserve was taken");
    assert_eq!(new_turn.reserved_credits_micro, None);
    for row in quota_rows(&app, u.tenant, u.user).await {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
    }
    assert_eq!(provider_calls(&app).len(), 1);

    // The chat is not blocked by a running turn.
    app.usage.set_catalog(test_catalog());
    send_turn(&app, &u, chat, json!({"content": "hi"})).await;
    wait_for_payloads(&app, USAGE_QUEUE, 2).await;
    wait_for_payloads(&app, AUDIT_QUEUE, 3).await;
    let usage = app.outbox_payloads(USAGE_QUEUE);
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert!(
        usage.iter().all(|e| e["turn_id"] != json!(new_turn.id)),
        "{usage:?}"
    );
    let mut audit: Vec<String> = app
        .outbox_payloads(AUDIT_QUEUE)
        .iter()
        .map(|a| a["event_type"].as_str().unwrap().to_owned())
        .collect();
    audit.sort();
    assert_eq!(audit, ["turn_completed", "turn_completed", "turn_retry"]);
}

#[tokio::test]
async fn mutation_of_summarized_turn_drops_summary() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 1, 1));
    let first = send_turn(&app, &u, chat, json!({"content": "q1"})).await;
    let second = send_turn(&app, &u, chat, json!({"content": "q2"})).await;
    let messages = messages_of(&app, chat).await;
    assert_eq!(messages.len(), 4);
    // Frontier: the first turn's answer (before the second turn's question).
    seed_thread_summary(&app, &messages[1]).await;
    compress_messages(&app, chat).await;

    // The summary does not cover the second turn's question: kept.
    assert_eq!(delete(&app, &u, chat, second).await.status, 204);
    assert!(thread_summary_of(&app, chat).await.is_some());
    assert!(
        messages_of(&app, chat)
            .await
            .iter()
            .all(|m| m.is_compressed)
    );

    // It covers the first turn's question: dropped, compression cleared.
    assert_eq!(delete(&app, &u, chat, first).await.status, 204);
    assert!(thread_summary_of(&app, chat).await.is_none());
    assert!(
        messages_of(&app, chat)
            .await
            .iter()
            .all(|m| !m.is_compressed)
    );
}

#[tokio::test]
async fn retry_reapplies_image_guards() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let image = seed_attachment(&app, SeedAttachment::image(u.tenant, chat, u.user)).await;
    script_provider(&app, answer(&["a cat"], 1, 1));
    let old = send_turn(
        &app,
        &u,
        chat,
        json!({"content": "what is it?", "attachment_ids": [image]}),
    )
    .await;
    app.usage.set_kill_switches(mini_chat_sdk::KillSwitches {
        disable_images: true,
        ..NO_KILL_SWITCHES
    });

    for res in [
        rejected(retry(&app, &u, chat, old).await),
        rejected(edit(&app, &u, chat, old, "and now?").await),
    ] {
        assert_eq!(res.status, 400, "{}", res.json);
        let v = &res.json["context"]["violations"][0];
        assert_eq!(v["type"], "FEATURE_DISABLED", "{}", res.json);
        assert_eq!(v["subject"], "images", "{}", res.json);
    }

    let turns = turns_of(&app, chat).await;
    assert_eq!(turns.len(), 1);
    assert!(turns[0].deleted_at.is_none());
    assert_eq!(provider_calls(&app).len(), 1);
}

/// Retry and edit re-submit the replaced turn's `web_search` request flag: the new turn keeps
/// `web_search_enabled` and its provider request carries the `web_search` tool.
#[tokio::test]
async fn retry_and_edit_reuse_the_web_search_flag() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 3, 2));
    let has_web_search = |call: &Value| {
        call["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["type"] == "web_search"))
    };

    let searched = send_turn(
        &app,
        &u,
        chat,
        json!({"content": "news?", "web_search": {"enabled": true}}),
    )
    .await;
    let retried = completed_request(&accepted(retry(&app, &u, chat, searched).await));
    let edited = completed_request(&accepted(
        edit(&app, &u, chat, retried, "news today?").await,
    ));
    let plain = send_turn(&app, &u, chat, json!({"content": "thanks"})).await;
    let plain_retried = completed_request(&accepted(retry(&app, &u, chat, plain).await));

    for (request_id, enabled) in [
        (searched, true),
        (retried, true),
        (edited, true),
        (plain, false),
        (plain_retried, false),
    ] {
        assert_eq!(
            turn_of(&app, chat, request_id).await.web_search_enabled,
            enabled,
            "{request_id}"
        );
    }
    let calls = provider_calls(&app);
    assert_eq!(calls.len(), 5);
    let tools: Vec<bool> = calls.iter().map(has_web_search).collect();
    assert_eq!(tools, [true, true, true, false, false]);
}
