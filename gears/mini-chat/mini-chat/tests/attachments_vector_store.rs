#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The chat vector store creation protocol of an upload (DESIGN table `chat_vector_stores`,
//! "Creation protocol"): the placeholder row, the winner and loser paths, the stale placeholder
//! reclaim and the provider mismatch.

mod attachments;
mod common;

use attachments::{
    call_lines, calls_to, chat_rows, expect_created, file_path, script_file, script_store_add,
    script_store_create, store_rows, upload_pdf, vector_store_files_path, vector_stores_path,
    wait_calls,
};
use common::{assert_problem, blob, env_user_chat, exec_sql, ts_literal};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

/// Inserts a vector store row of the chat: `vector_store_id` `None` is a creation in progress.
async fn insert_store(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    vector_store: Option<&str>,
    provider: &str,
    created_at: OffsetDateTime,
) {
    let vector_store = vector_store.map_or_else(|| "NULL".to_owned(), |id| format!("'{id}'"));
    exec_sql(
        env,
        &format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, \
             file_count, created_at) VALUES ({}, {}, {}, {vector_store}, '{provider}', 0, {})",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(Uuid::parse_str(chat).unwrap()),
            ts_literal(created_at),
        ),
    )
    .await;
}

#[tokio::test]
async fn a_placeholder_older_than_120_seconds_is_reclaimed_and_the_store_is_created() {
    let (env, user, chat) = env_user_chat().await;
    insert_store(
        &env,
        &user,
        &chat,
        None,
        "openai",
        OffsetDateTime::now_utc() - time::Duration::seconds(121),
    )
    .await;
    script_file(&env, "file-doc1");
    script_store_create(&env, "vs_new");
    script_store_add(&env, "vs_new", "file-doc1", Some("completed"));

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    assert_eq!(detail["status"], json!("ready"));
    let stores = store_rows(&env, &chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_new"));
    assert_eq!(calls_to(&env, "POST", &vector_stores_path()).len(), 1);
}

#[tokio::test]
async fn a_loser_waits_for_the_winner_and_uses_its_store_without_creating_one() {
    let (env, user, chat) = env_user_chat().await;
    insert_store(
        &env,
        &user,
        &chat,
        None,
        "openai",
        OffsetDateTime::now_utc(),
    )
    .await;
    script_file(&env, "file-doc1");
    script_store_add(&env, "vs_winner", "file-doc1", Some("completed"));
    let publish = async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        exec_sql(
            &env,
            "UPDATE chat_vector_stores SET vector_store_id = 'vs_winner' WHERE vector_store_id IS NULL",
        )
        .await;
    };

    let (response, ()) = tokio::join!(upload_pdf(&env, &user, &chat), publish);

    let detail = expect_created(&response);
    assert_eq!(detail["status"], json!("ready"));
    assert!(
        calls_to(&env, "POST", &vector_stores_path()).is_empty(),
        "the loser never calls the provider create: {:?}",
        call_lines(&env)
    );
    assert_eq!(
        calls_to(&env, "POST", &vector_store_files_path("vs_winner")).len(),
        1
    );
}

#[tokio::test]
async fn a_loser_gives_up_with_503_when_the_winner_never_publishes_its_store() {
    let (env, user, chat) = env_user_chat().await;
    insert_store(
        &env,
        &user,
        &chat,
        None,
        "openai",
        OffsetDateTime::now_utc(),
    )
    .await;
    script_file(&env, "file-doc1");

    let resp = upload_pdf(&env, &user, &chat).await;

    attachments::assert_unavailable(&resp, 10);
    assert!(calls_to(&env, "POST", &vector_stores_path()).is_empty());
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("vector_store_failed"));
    wait_calls(&env, "DELETE", &file_path("file-doc1"), 1).await;
    assert_eq!(
        store_rows(&env, &chat).await.len(),
        1,
        "the winner's placeholder is not ours to delete"
    );
}

#[tokio::test]
async fn a_store_created_for_another_backend_is_409_provider_mismatch() {
    let (env, user, chat) = env_user_chat().await;
    insert_store(
        &env,
        &user,
        &chat,
        Some("vs_other"),
        "azure-eu",
        OffsetDateTime::now_utc(),
    )
    .await;
    script_file(&env, "file-doc1");

    let resp = upload_pdf(&env, &user, &chat).await;

    let problem = assert_problem(&resp, 409, Some("already_exists"));
    assert_eq!(
        problem["context"]["resource_name"],
        json!("provider_mismatch")
    );
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("vector_store_failed"));
    assert!(
        calls_to(&env, "POST", &vector_store_files_path("vs_other")).is_empty(),
        "the file is not added to a store of another backend"
    );
    wait_calls(&env, "DELETE", &file_path("file-doc1"), 1).await;
}
