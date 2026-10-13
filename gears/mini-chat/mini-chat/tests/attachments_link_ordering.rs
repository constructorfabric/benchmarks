#![allow(clippy::unwrap_used, clippy::expect_used)]

//! An attachment delete and the turns that link attachments are serialized on the chat row (DESIGN
//! "Attachment Deletion" and the mutation invariants): the delete takes the chat row lock before it
//! counts the references, a send and a retry take it before they validate and insert the links, so
//! a submitted message never references a soft-deleted attachment. Ordering is asserted with
//! SQLite triggers that log the writes in execution order.

mod attachments;
mod common;
mod mutation;

use attachments::{delete_attachment, detail_id, upload_ready_image};
use common::{
    env_user_chat, exec_sql, expect_stream, scalar_text, send, stream_request_id, terminal,
};
use http::StatusCode;
use mini_chat::test_support::TestEnv;
use serde_json::json;

async fn log_writes(env: &TestEnv) {
    for sql in [
        "CREATE TABLE write_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, what TEXT NOT NULL)",
        "CREATE TRIGGER log_chat_write AFTER UPDATE ON chats \
         BEGIN INSERT INTO write_log (what) VALUES ('chat'); END",
        "CREATE TRIGGER log_link AFTER INSERT ON message_attachments \
         BEGIN INSERT INTO write_log (what) VALUES ('link'); END",
        "CREATE TRIGGER log_attachment_deleted AFTER UPDATE ON attachments \
         WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL \
         BEGIN INSERT INTO write_log (what) VALUES ('attachment_deleted'); END",
    ] {
        exec_sql(env, sql).await;
    }
}

async fn clear_log(env: &TestEnv) {
    exec_sql(env, "DELETE FROM write_log").await;
}

async fn log(env: &TestEnv) -> Vec<String> {
    scalar_text(
        env,
        "SELECT COALESCE(group_concat(what, ','), '') AS t \
         FROM (SELECT what FROM write_log ORDER BY seq)",
    )
    .await
    .split(',')
    .filter(|what| !what.is_empty())
    .map(str::to_owned)
    .collect()
}

fn position(log: &[String], what: &str) -> usize {
    log.iter()
        .position(|entry| entry == what)
        .unwrap_or_else(|| panic!("no `{what}` write in {log:?}"))
}

#[tokio::test]
async fn a_send_writes_the_chat_row_before_it_links_attachments() {
    let (env, user, chat) = env_user_chat().await;
    let image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img").await);
    log_writes(&env).await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done");
    let log = log(&env).await;
    assert_eq!(
        log.first().map(String::as_str),
        Some("chat"),
        "the chat row lock is taken first: {log:?}"
    );
    assert!(position(&log, "chat") < position(&log, "link"), "{log:?}");
}

#[tokio::test]
async fn a_retry_writes_the_chat_row_before_it_copies_the_attachment_links() {
    let (env, user, chat) = env_user_chat().await;
    let image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img").await);
    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );
    log_writes(&env).await;
    mutation::prepare(&env, mutation::Op::Retry);

    let retried = mutation::retry(&env, &user, &chat, stream_request_id(&events)).await;

    mutation::expect_mutated(&retried, mutation::Op::Retry);
    let log = log(&env).await;
    assert!(position(&log, "chat") < position(&log, "link"), "{log:?}");
}

#[tokio::test]
async fn an_attachment_delete_writes_the_chat_row_before_it_soft_deletes() {
    let (env, user, chat) = env_user_chat().await;
    let image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img").await);
    log_writes(&env).await;

    attachments::delete_ok(&env, &user, &chat, image).await;

    let log = log(&env).await;
    assert_eq!(log.first().map(String::as_str), Some("chat"), "{log:?}");
    assert!(
        position(&log, "chat") < position(&log, "attachment_deleted"),
        "{log:?}"
    );
}

#[tokio::test]
async fn a_refused_attachment_delete_still_took_the_chat_row_lock_before_the_reference_count() {
    let (env, user, chat) = env_user_chat().await;
    let image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img").await);
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );
    log_writes(&env).await;
    clear_log(&env).await;

    let resp = delete_attachment(&env, &user, &chat, &image.to_string()).await;

    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "the attachment is in use"
    );
    let log = log(&env).await;
    assert_eq!(
        log,
        vec!["chat".to_owned()],
        "the lock is the first statement: it is taken although the delete is refused"
    );
}
