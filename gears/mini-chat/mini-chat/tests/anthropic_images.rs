#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The images of an Anthropic chat reach the Messages API by their secondary copy (DESIGN
//! "Internal: Multimodal Input Format": the turn uses the Anthropic file-id map; the Anthropic
//! adapter drops only image blocks without an Anthropic file id), on send, retry and edit.

mod attachments;
mod common;
mod mutation;

use attachments::{PNG, detail_id, expect_created, png, provider_error, script_plain_file, upload};
use common::{
    STANDARD_MODEL, catalog_entry, chat_on, expect_stream, merge, policy, send, stream_request_id,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const SECONDARY: &str = "file_011secondary";

async fn anthropic_env() -> (TestEnv, TestUser, String) {
    let claude = merge(
        catalog_entry("claude", "Premium", true, true),
        json!({ "provider_id": "anthropic" }),
    );
    let env = TestEnvBuilder::new()
        .gear_config(json!({
            "providers": { "anthropic": {
                "kind": "anthropic_messages",
                "host": "anthropic.test",
                "api_path": "/v1/messages",
                "storage_kind": "openai",
                "rag_provider": "openai",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {
                    "header": "x-api-key", "prefix": "", "secret_ref": "cred://anthropic-key"
                }
            } }
        }))
        .policy_config(policy(vec![
            claude,
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ]))
        .build()
        .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, "claude").await;
    (env, user, chat)
}

/// A completed Messages API stream with one text delta.
fn anthropic_answer(text: &str) -> ScriptedResponse {
    ScriptedResponse::Sse(vec![
        (
            "message_start".to_owned(),
            json!({ "type": "message_start", "message": {
                "id": "msg_01", "usage": { "input_tokens": 10, "output_tokens": 1 }
            } }),
        ),
        (
            "content_block_delta".to_owned(),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": text } }),
        ),
        (
            "message_delta".to_owned(),
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 4 } }),
        ),
        ("message_stop".to_owned(), json!({ "type": "message_stop" })),
    ])
}

fn answer(env: &TestEnv) {
    env.provider()
        .enqueue("/v1/messages", anthropic_answer("fine"));
}

/// Uploads a PNG whose secondary copy is `secondary` (`None`: the copy fails).
async fn upload_image(env: &TestEnv, user: &TestUser, chat: &str, secondary: Option<&str>) -> Uuid {
    script_plain_file(env, "file-primary");
    let scripted = match secondary {
        Some(id) => attachments::ok(json!({ "id": id, "type": "file" })),
        None => provider_error(500),
    };
    env.provider().enqueue("/anthropic.test/", scripted);
    detail_id(&expect_created(
        &upload(env, user, chat, "a.png", PNG, &png(64, 64)).await,
    ))
}

/// The last `/v1/messages` body the provider got.
fn last_messages_body(env: &TestEnv) -> Value {
    env.provider()
        .chat_requests()
        .last()
        .and_then(|request| request.body_json.clone())
        .expect("a Messages API request")
}

/// The `content` of the last (current) message of a Messages API body.
fn current_content(body: &Value) -> Value {
    body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .get("content")
        .unwrap()
        .clone()
}

fn assert_image_block(content: &Value, text: &str) {
    assert_eq!(
        content,
        &json!([
            { "type": "text", "text": text },
            { "type": "image", "source": { "type": "file", "file_id": SECONDARY } }
        ])
    );
}

#[tokio::test]
async fn a_send_sends_the_image_by_its_secondary_copy() {
    let (env, user, chat) = anthropic_env().await;
    let image = upload_image(&env, &user, &chat, Some(SECONDARY)).await;
    answer(&env);

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );

    assert_eq!(common::terminal(&events).event, "done", "{events:?}");
    let request = env.provider().chat_requests().pop().unwrap();
    assert_image_block(&current_content(&last_messages_body(&env)), "look");
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name == "anthropic-beta" && value == "files-api-2025-04-14"),
        "a file source needs the Files API beta: {:?}",
        request.headers
    );
    assert!(
        !request
            .body_json
            .unwrap()
            .to_string()
            .contains("file-primary"),
        "the primary file id is never sent to Anthropic"
    );
}

#[tokio::test]
async fn retry_and_edit_send_the_image_by_its_secondary_copy() {
    let (env, user, chat) = anthropic_env().await;
    let image = upload_image(&env, &user, &chat, Some(SECONDARY)).await;
    answer(&env);
    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );

    answer(&env);
    let retried = mutation::retry(&env, &user, &chat, stream_request_id(&events)).await;
    let retried_events = mutation::expect_mutated(&retried, mutation::Op::Retry);
    assert_image_block(&current_content(&last_messages_body(&env)), "look");

    answer(&env);
    let edited = mutation::edit(
        &env,
        &user,
        &chat,
        stream_request_id(&retried_events),
        "look again",
    )
    .await;
    mutation::expect_mutated(&edited, mutation::Op::Edit);
    assert_image_block(&current_content(&last_messages_body(&env)), "look again");
}

#[tokio::test]
async fn an_image_without_a_secondary_copy_is_dropped_from_the_request() {
    let (env, user, chat) = anthropic_env().await;
    let image = upload_image(&env, &user, &chat, None).await;
    answer(&env);

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );

    assert_eq!(common::terminal(&events).event, "done", "{events:?}");
    assert_eq!(
        current_content(&last_messages_body(&env)),
        json!("look"),
        "no image block without an Anthropic file id"
    );
}
