#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The images of a turn reach the provider in the order of the request's `attachment_ids`, also
//! when the turn is retried or edited (DESIGN "Internal: Multimodal Input Format": multiple
//! images are appended in order).

mod attachments;
mod common;
mod mutation;

use attachments::{detail_id, upload_ready_image};
use common::{chat_request_body, env_user_chat, expect_stream, send, stream_request_id};
use serde_json::{Value, json};
use uuid::Uuid;

/// The provider file ids of the `input_image` items of the current message of a chat request.
fn image_files(body: &Value) -> Vec<String> {
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == json!("input_image"))
        .map(|item| item["file_id"].as_str().unwrap().to_owned())
        .collect()
}

/// Three uploaded images; returns `(attachment id, provider file id)` in an order that differs
/// from the id order, so that a sort by id would be visible.
async fn three_images(
    env: &mini_chat::test_support::TestEnv,
    user: &mini_chat::test_support::TestUser,
    chat: &str,
) -> Vec<(Uuid, String)> {
    let mut images = Vec::new();
    for index in 0..3 {
        let file = format!("file-img{index}");
        let detail = upload_ready_image(env, user, chat, &format!("{index}.png"), &file).await;
        images.push((detail_id(&detail), file));
    }
    images.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    images
}

#[tokio::test]
async fn images_follow_the_order_of_the_request_on_send_retry_and_edit() {
    let (env, user, chat) = env_user_chat().await;
    let images = three_images(&env, &user, &chat).await;
    let expected: Vec<String> = images.iter().map(|(_, file)| file.clone()).collect();
    let ids: Vec<String> = images.iter().map(|(id, _)| id.to_string()).collect();

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "compare", "attachment_ids": ids }),
        )
        .await,
    );
    assert_eq!(image_files(&chat_request_body(&env, 0)), expected, "send");

    let request_id = stream_request_id(&events);
    let retried = mutation::retry(&env, &user, &chat, request_id).await;
    let retried_events = mutation::expect_mutated(&retried, mutation::Op::Retry);
    assert_eq!(
        image_files(&mutation::last_chat_body(&env)),
        expected,
        "retry"
    );

    let edited = mutation::edit(
        &env,
        &user,
        &chat,
        stream_request_id(&retried_events),
        "edited",
    )
    .await;
    mutation::expect_mutated(&edited, mutation::Op::Edit);
    assert_eq!(
        image_files(&mutation::last_chat_body(&env)),
        expected,
        "edit"
    );
}
