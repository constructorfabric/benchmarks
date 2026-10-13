#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-run-migrations` and
//! `cpt-cf-mini-chat-dod-gear-foundation-schema` on in-memory `SQLite`.

use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use uuid::Uuid;

use super::Migrator;
use crate::infra::db::test_support::{
    Row, TS, TestDb, attachment_row, chat_row, message_row, turn_row,
};

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

const TENANT: u128 = 0x7E;
const USER: u128 = 0x55;

fn tenant() -> Uuid {
    id(TENANT)
}

fn user() -> Uuid {
    id(USER)
}

/// Fixture with chat `1` already inserted.
async fn db_with_chat() -> TestDb {
    let db = TestDb::migrated().await;
    db.insert("chats", chat_row(id(1), tenant(), user()))
        .await
        .expect("insert chat");
    db
}

fn reaction_row(rid: Uuid, message: Uuid, reaction: &str) -> Row {
    Row::default()
        .set("id", rid)
        .set("message_id", message)
        .set("user_id", user())
        .set("tenant_id", tenant())
        .set("reaction", reaction)
        .set("created_at", TS)
}

fn quota_row(rid: Uuid, bucket: &str) -> Row {
    Row::default()
        .set("id", rid)
        .set("tenant_id", tenant())
        .set("user_id", user())
        .set("period_type", "daily")
        .set("period_start", "2026-01-01")
        .set("bucket", bucket)
        .set("updated_at", TS)
}

fn vector_store_row(rid: Uuid, chat: Uuid) -> Row {
    Row::default()
        .set("id", rid)
        .set("tenant_id", tenant())
        .set("chat_id", chat)
        .set("provider", "openai")
        .set("created_at", TS)
}

fn summary_row(rid: Uuid, chat: Uuid) -> Row {
    Row::default()
        .set("id", rid)
        .set("tenant_id", tenant())
        .set("chat_id", chat)
        .set("summary_text", "summary")
        .set("summarized_up_to_created_at", TS)
        .set("summarized_up_to_message_id", id(100))
        .set("token_estimate", 0_i64)
        .set("created_at", TS)
        .set("updated_at", TS)
}

fn message_attachment_row(chat: Uuid, message: Uuid, attachment: Uuid) -> Row {
    Row::default()
        .set("tenant_id", tenant())
        .set("chat_id", chat)
        .set("message_id", message)
        .set("attachment_id", attachment)
        .set("created_at", TS)
}

// ---- schema presence --------------------------------------------------------

#[tokio::test]
async fn creates_all_p1_tables() {
    let db = TestDb::migrated().await;
    let tables = db.table_names().await;

    for expected in [
        "chats",
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
        "message_reactions",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "table `{expected}` missing; found {tables:?}"
        );
    }
}

#[tokio::test]
async fn includes_shared_outbox_tables_with_gear_prefix() {
    let db = TestDb::migrated().await;
    let tables = db.table_names().await;

    for suffix in [
        "body",
        "partitions",
        "incoming",
        "outgoing",
        "dead_letters",
        "processor",
    ] {
        let expected = format!("mini_chat__outbox_{suffix}");
        assert!(
            tables.contains(&expected),
            "outbox table `{expected}` missing; found {tables:?}"
        );
    }
}

#[tokio::test]
async fn creates_no_mcp_or_tenant_closure_tables() {
    let db = TestDb::migrated().await;
    let tables = db.table_names().await;

    for forbidden in [
        "mcp_servers",
        "mcp_server_tools",
        "role_mcp_servers",
        "tenant_closure",
    ] {
        assert!(
            !tables.iter().any(|t| t == forbidden),
            "table `{forbidden}` must not exist"
        );
    }
    assert!(
        tables.iter().any(|t| t == "chats"),
        "the schema must exist for this check to mean anything"
    );
}

#[tokio::test]
async fn rerunning_migrations_is_a_no_op() {
    let db = TestDb::migrated().await;
    let tables_before = db.table_names().await;

    let second = run_migrations_for_testing(&db.db, Migrator::migrations())
        .await
        .expect("second run succeeds");

    assert_eq!(second.applied, 0, "nothing is applied twice");
    assert!(
        second.skipped >= 1,
        "the migrations are recorded as applied"
    );
    assert_eq!(db.table_names().await, tables_before);
}

// ---- chats -------------------------------------------------------------------

#[tokio::test]
async fn chats_default_is_temporary_to_false() {
    let db = TestDb::migrated().await;
    db.insert(
        "chats",
        chat_row(id(1), tenant(), user()).unset("is_temporary"),
    )
    .await
    .expect("insert chat without is_temporary");

    assert_eq!(db.query_i64("SELECT is_temporary FROM chats").await, 0);
}

#[tokio::test]
async fn chats_require_tenant_user_and_timestamps() {
    let db = TestDb::migrated().await;
    db.insert("chats", chat_row(id(2), tenant(), user()))
        .await
        .expect("control row with every required column");

    for column in ["tenant_id", "user_id", "created_at", "updated_at"] {
        let result = db
            .insert("chats", chat_row(id(1), tenant(), user()).null(column))
            .await;
        assert!(result.is_err(), "NULL `{column}` must be rejected");
    }
}

// ---- messages ----------------------------------------------------------------

#[tokio::test]
async fn messages_apply_documented_defaults() {
    let db = db_with_chat().await;
    db.insert("messages", message_row(id(10), tenant(), id(1)))
        .await
        .expect("insert message");

    assert_eq!(db.query_i64("SELECT input_tokens FROM messages").await, 0);
    assert_eq!(db.query_i64("SELECT output_tokens FROM messages").await, 0);
    assert_eq!(
        db.query_string("SELECT features_used FROM messages").await,
        "[]"
    );
}

#[tokio::test]
async fn messages_require_chat_role_content_and_timestamp() {
    let db = db_with_chat().await;

    for column in [
        "tenant_id",
        "chat_id",
        "role",
        "content",
        "content_type",
        "created_at",
    ] {
        let result = db
            .insert(
                "messages",
                message_row(id(10), tenant(), id(1)).null(column),
            )
            .await;
        assert!(result.is_err(), "NULL `{column}` must be rejected");
    }
}

#[tokio::test]
async fn messages_require_an_existing_chat() {
    let db = db_with_chat().await;
    db.insert("messages", message_row(id(11), tenant(), id(1)))
        .await
        .expect("control: message of an existing chat");

    let result = db
        .insert("messages", message_row(id(10), tenant(), id(999)))
        .await;

    assert!(result.is_err(), "FK chat_id must reject an unknown chat");
}

#[tokio::test]
async fn messages_reject_duplicate_request_id_and_role_in_a_chat() {
    let db = db_with_chat().await;
    let request = id(500);
    db.insert(
        "messages",
        message_row(id(10), tenant(), id(1)).set("request_id", request),
    )
    .await
    .expect("first user message");

    let duplicate = db
        .insert(
            "messages",
            message_row(id(11), tenant(), id(1)).set("request_id", request),
        )
        .await;

    assert!(
        duplicate.is_err(),
        "(chat_id, request_id, role) must be unique"
    );
}

#[tokio::test]
async fn messages_allow_one_message_per_role_for_a_request() {
    let db = db_with_chat().await;
    let request = id(500);
    db.insert(
        "messages",
        message_row(id(10), tenant(), id(1)).set("request_id", request),
    )
    .await
    .expect("user message");

    db.insert(
        "messages",
        message_row(id(11), tenant(), id(1))
            .set("request_id", request)
            .set("role", "assistant"),
    )
    .await
    .expect("assistant message with the same request id");
}

#[tokio::test]
async fn messages_unique_index_ignores_null_request_ids_and_soft_deleted_rows() {
    let db = db_with_chat().await;
    let request = id(500);

    db.insert("messages", message_row(id(10), tenant(), id(1)))
        .await
        .expect("message without request id");
    db.insert("messages", message_row(id(11), tenant(), id(1)))
        .await
        .expect("a second message without request id");

    db.insert(
        "messages",
        message_row(id(12), tenant(), id(1)).set("request_id", request),
    )
    .await
    .expect("message with request id");
    db.soft_delete("messages", id(12)).await;
    db.insert(
        "messages",
        message_row(id(13), tenant(), id(1)).set("request_id", request),
    )
    .await
    .expect("a replacement after the soft delete");
}

#[tokio::test]
async fn messages_allow_the_same_request_id_in_another_chat() {
    let db = db_with_chat().await;
    db.insert("chats", chat_row(id(2), tenant(), user()))
        .await
        .expect("second chat");
    let request = id(500);

    db.insert(
        "messages",
        message_row(id(10), tenant(), id(1)).set("request_id", request),
    )
    .await
    .expect("message in chat 1");
    db.insert(
        "messages",
        message_row(id(11), tenant(), id(2)).set("request_id", request),
    )
    .await
    .expect("message in chat 2");
}

// ---- chat_turns --------------------------------------------------------------

#[tokio::test]
async fn chat_turns_apply_documented_defaults() {
    let db = db_with_chat().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "running"),
    )
    .await
    .expect("insert turn");

    assert_eq!(
        db.query_i64("SELECT web_search_enabled FROM chat_turns")
            .await,
        0
    );
    for column in [
        "web_search_completed_count",
        "code_interpreter_completed_count",
        "file_search_completed_count",
    ] {
        assert_eq!(
            db.query_i64(&format!("SELECT {column} FROM chat_turns"))
                .await,
            0,
            "{column} defaults to 0"
        );
    }
}

#[tokio::test]
async fn chat_turns_reject_duplicate_request_id_in_a_chat() {
    let db = db_with_chat().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "completed"),
    )
    .await
    .expect("first turn");

    let duplicate = db
        .insert(
            "chat_turns",
            turn_row(id(21), tenant(), id(1), id(500), "completed"),
        )
        .await;

    assert!(duplicate.is_err(), "UNIQUE (chat_id, request_id)");
}

#[tokio::test]
async fn chat_turns_reject_unknown_state_and_requester_type() {
    let db = db_with_chat().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "completed"),
    )
    .await
    .expect("control row with valid values");

    let bad_state = db
        .insert(
            "chat_turns",
            turn_row(id(21), tenant(), id(1), id(501), "completed").set("state", "paused"),
        )
        .await;
    let bad_requester = db
        .insert(
            "chat_turns",
            turn_row(id(22), tenant(), id(1), id(502), "completed").set("requester_type", "robot"),
        )
        .await;

    assert!(bad_state.is_err(), "CHECK state IN (...)");
    assert!(
        bad_requester.is_err(),
        "CHECK requester_type IN ('user','system')"
    );
}

#[tokio::test]
async fn chat_turns_accept_every_valid_state_and_requester_type() {
    let db = db_with_chat().await;
    for (n, state) in ["completed", "failed", "cancelled"].iter().enumerate() {
        db.insert(
            "chat_turns",
            turn_row(
                id(20 + n as u128),
                tenant(),
                id(1),
                id(500 + n as u128),
                state,
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("state `{state}` must be accepted: {e}"));
    }
    db.insert(
        "chat_turns",
        turn_row(id(30), tenant(), id(1), id(600), "running").set("requester_type", "system"),
    )
    .await
    .expect("requester_type `system` and state `running`");
}

#[tokio::test]
async fn chat_turns_reject_negative_completed_counts() {
    let db = db_with_chat().await;

    for (n, column) in [
        "web_search_completed_count",
        "code_interpreter_completed_count",
        "file_search_completed_count",
    ]
    .iter()
    .enumerate()
    {
        let result = db
            .insert(
                "chat_turns",
                turn_row(
                    id(20 + n as u128),
                    tenant(),
                    id(1),
                    id(500 + n as u128),
                    "completed",
                )
                .set(column, -1_i64),
            )
            .await;
        assert!(result.is_err(), "negative `{column}` must be rejected");
    }
}

#[tokio::test]
async fn chat_turns_allow_at_most_one_running_turn_per_chat() {
    let db = db_with_chat().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "running"),
    )
    .await
    .expect("first running turn");

    let second = db
        .insert(
            "chat_turns",
            turn_row(id(21), tenant(), id(1), id(501), "running"),
        )
        .await;

    assert!(second.is_err(), "partial UNIQUE (chat_id) WHERE running");
}

#[tokio::test]
async fn running_turn_uniqueness_ignores_other_chats_terminal_and_soft_deleted_turns() {
    let db = db_with_chat().await;
    db.insert("chats", chat_row(id(2), tenant(), user()))
        .await
        .expect("second chat");
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "running"),
    )
    .await
    .expect("running turn in chat 1");

    db.insert(
        "chat_turns",
        turn_row(id(21), tenant(), id(2), id(501), "running"),
    )
    .await
    .expect("running turn in another chat");
    db.insert(
        "chat_turns",
        turn_row(id(22), tenant(), id(1), id(502), "completed"),
    )
    .await
    .expect("terminal turn next to a running one");

    db.soft_delete("chat_turns", id(20)).await;
    db.insert(
        "chat_turns",
        turn_row(id(23), tenant(), id(1), id(503), "running"),
    )
    .await
    .expect("a new running turn after the old one was soft-deleted");
}

#[tokio::test]
async fn chat_turns_cross_column_invariants_are_left_to_repositories() {
    let db = db_with_chat().await;

    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "completed").null("completed_at"),
    )
    .await
    .expect("terminal turn without completed_at is not rejected by the database");
    db.insert(
        "chat_turns",
        turn_row(id(21), tenant(), id(1), id(501), "running").null("last_progress_at"),
    )
    .await
    .expect("running turn without last_progress_at is not rejected by the database");
}

// ---- attachments -------------------------------------------------------------

#[tokio::test]
async fn attachments_apply_documented_defaults() {
    let db = db_with_chat().await;
    db.insert(
        "attachments",
        attachment_row(id(30), tenant(), id(1))
            .unset("for_code_interpreter")
            .unset("updated_at"),
    )
    .await
    .expect("insert attachment relying on defaults");

    assert_eq!(
        db.query_string("SELECT storage_backend FROM attachments")
            .await,
        "azure"
    );
    assert_eq!(
        db.query_string("SELECT secondary_status FROM attachments")
            .await,
        "not_attempted"
    );
    assert_eq!(
        db.query_i64("SELECT cleanup_attempts FROM attachments")
            .await,
        0
    );
    assert_eq!(
        db.query_i64("SELECT for_code_interpreter FROM attachments")
            .await,
        0
    );
    assert!(
        !db.query_string("SELECT CAST(updated_at AS TEXT) FROM attachments")
            .await
            .is_empty(),
        "updated_at defaults to now"
    );
}

#[tokio::test]
async fn attachments_reject_values_outside_their_check_sets() {
    let db = db_with_chat().await;
    db.insert("attachments", attachment_row(id(30), tenant(), id(1)))
        .await
        .expect("control row with valid values");

    for (column, bad) in [
        ("attachment_kind", "video"),
        ("status", "archived"),
        ("secondary_status", "unknown"),
        ("secondary_provider_kind", "openai"),
    ] {
        let result = db
            .insert(
                "attachments",
                attachment_row(id(31), tenant(), id(1)).set(column, bad),
            )
            .await;
        assert!(result.is_err(), "`{column}` = `{bad}` must be rejected");
    }
}

#[tokio::test]
async fn attachments_accept_every_valid_check_value() {
    let db = db_with_chat().await;
    let mut n = 40_u128;
    let mut next = || {
        n += 1;
        id(n)
    };

    for (column, value) in [
        ("attachment_kind", "document"),
        ("attachment_kind", "image"),
        ("status", "pending"),
        ("status", "uploaded"),
        ("status", "ready"),
        ("status", "failed"),
        ("secondary_status", "not_attempted"),
        ("secondary_status", "pending"),
        ("secondary_status", "uploaded"),
        ("secondary_status", "failed"),
        ("secondary_provider_kind", "anthropic"),
    ] {
        db.insert(
            "attachments",
            attachment_row(next(), tenant(), id(1)).set(column, value),
        )
        .await
        .unwrap_or_else(|e| panic!("`{column}` = `{value}` must be accepted: {e}"));
    }
    db.insert(
        "attachments",
        attachment_row(next(), tenant(), id(1)).null("secondary_provider_kind"),
    )
    .await
    .expect("NULL secondary_provider_kind");
}

#[tokio::test]
async fn attachments_do_not_constrain_cleanup_status_values() {
    let db = db_with_chat().await;

    db.insert(
        "attachments",
        attachment_row(id(30), tenant(), id(1)).set("cleanup_status", "whatever"),
    )
    .await
    .expect("cleanup_status is kept by the repositories, not by a CHECK");
}

// ---- message_attachments -----------------------------------------------------

async fn db_with_message_and_attachment() -> TestDb {
    let db = db_with_chat().await;
    db.insert("messages", message_row(id(10), tenant(), id(1)))
        .await
        .expect("message");
    db.insert("attachments", attachment_row(id(30), tenant(), id(1)))
        .await
        .expect("attachment");
    db
}

#[tokio::test]
async fn message_attachments_link_a_message_and_an_attachment_of_the_same_chat() {
    let db = db_with_message_and_attachment().await;

    db.insert(
        "message_attachments",
        message_attachment_row(id(1), id(10), id(30)),
    )
    .await
    .expect("link");
    let duplicate = db
        .insert(
            "message_attachments",
            message_attachment_row(id(1), id(10), id(30)),
        )
        .await;

    assert!(
        duplicate.is_err(),
        "PK (chat_id, message_id, attachment_id)"
    );
}

#[tokio::test]
async fn message_attachments_reject_a_message_or_attachment_of_another_chat() {
    let db = db_with_message_and_attachment().await;
    db.insert("chats", chat_row(id(2), tenant(), user()))
        .await
        .expect("second chat");
    db.insert("messages", message_row(id(11), tenant(), id(2)))
        .await
        .expect("message of chat 2");
    db.insert("attachments", attachment_row(id(31), tenant(), id(2)))
        .await
        .expect("attachment of chat 2");

    let foreign_message = db
        .insert(
            "message_attachments",
            message_attachment_row(id(1), id(11), id(30)),
        )
        .await;
    let foreign_attachment = db
        .insert(
            "message_attachments",
            message_attachment_row(id(1), id(10), id(31)),
        )
        .await;

    assert!(
        foreign_message.is_err(),
        "composite FK (message_id, chat_id) -> messages(id, chat_id)"
    );
    assert!(
        foreign_attachment.is_err(),
        "composite FK (attachment_id, chat_id) -> attachments(id, chat_id)"
    );
}

#[tokio::test]
async fn deleting_a_message_or_an_attachment_removes_its_links() {
    let db = db_with_message_and_attachment().await;
    db.insert("attachments", attachment_row(id(31), tenant(), id(1)))
        .await
        .expect("second attachment");
    db.insert(
        "message_attachments",
        message_attachment_row(id(1), id(10), id(30)),
    )
    .await
    .expect("link 1");
    db.insert(
        "message_attachments",
        message_attachment_row(id(1), id(10), id(31)),
    )
    .await
    .expect("link 2");

    db.delete_by_id("attachments", id(30))
        .await
        .expect("delete attachment");
    assert_eq!(db.count("message_attachments").await, 1);

    db.delete_by_id("messages", id(10))
        .await
        .expect("delete message");
    assert_eq!(db.count("message_attachments").await, 0);
}

// ---- thread_summaries / chat_vector_stores -----------------------------------

#[tokio::test]
async fn thread_summaries_allow_one_summary_per_chat() {
    let db = db_with_chat().await;
    db.insert("thread_summaries", summary_row(id(40), id(1)))
        .await
        .expect("first summary");

    let second = db
        .insert("thread_summaries", summary_row(id(41), id(1)))
        .await;

    assert!(second.is_err(), "UNIQUE chat_id");
}

#[tokio::test]
async fn thread_summaries_require_both_frontier_columns() {
    let db = db_with_chat().await;

    for column in ["summarized_up_to_created_at", "summarized_up_to_message_id"] {
        let result = db
            .insert("thread_summaries", summary_row(id(40), id(1)).null(column))
            .await;
        assert!(result.is_err(), "NULL `{column}` must be rejected");
    }
}

#[tokio::test]
async fn chat_vector_stores_are_unique_per_tenant_and_chat() {
    let db = db_with_chat().await;
    db.insert("chat_vector_stores", vector_store_row(id(50), id(1)))
        .await
        .expect("first row (vector_store_id is nullable, file_count defaults)");

    let duplicate = db
        .insert("chat_vector_stores", vector_store_row(id(51), id(1)))
        .await;

    assert!(duplicate.is_err(), "UNIQUE (tenant_id, chat_id)");
    assert_eq!(
        db.query_i64("SELECT file_count FROM chat_vector_stores")
            .await,
        0
    );
}

#[tokio::test]
async fn chat_vector_stores_reject_a_negative_file_count() {
    let db = db_with_chat().await;

    let result = db
        .insert(
            "chat_vector_stores",
            vector_store_row(id(50), id(1)).set("file_count", -1_i64),
        )
        .await;

    assert!(result.is_err(), "CHECK file_count >= 0");
}

// ---- quota_usage -------------------------------------------------------------

#[tokio::test]
async fn quota_usage_key_is_unique_and_counters_default_to_zero() {
    let db = TestDb::migrated().await;
    db.insert("quota_usage", quota_row(id(60), "total"))
        .await
        .expect("first row");

    let duplicate = db.insert("quota_usage", quota_row(id(61), "total")).await;
    let other_bucket = db
        .insert("quota_usage", quota_row(id(62), "tier:premium"))
        .await;

    assert!(
        duplicate.is_err(),
        "UNIQUE (tenant_id, user_id, period_type, period_start, bucket)"
    );
    other_bucket.expect("another bucket of the same period is a different key");
    for column in [
        "spent_credits_micro",
        "reserved_credits_micro",
        "calls",
        "input_tokens",
        "output_tokens",
        "file_search_calls",
        "web_search_calls",
        "code_interpreter_calls",
        "rag_retrieval_calls",
        "image_inputs",
        "image_upload_bytes",
    ] {
        assert_eq!(
            db.query_i64(&format!(
                "SELECT {column} FROM quota_usage WHERE bucket = 'total'"
            ))
            .await,
            0,
            "{column} defaults to 0"
        );
    }
}

#[tokio::test]
async fn quota_usage_requires_a_bucket() {
    let db = TestDb::migrated().await;
    db.insert("quota_usage", quota_row(id(59), "total"))
        .await
        .expect("control row with a bucket");

    let result = db
        .insert("quota_usage", quota_row(id(60), "total").null("bucket"))
        .await;

    assert!(result.is_err(), "NOT NULL bucket");
}

// ---- message_reactions -------------------------------------------------------

async fn db_with_message() -> TestDb {
    let db = db_with_chat().await;
    db.insert(
        "messages",
        message_row(id(10), tenant(), id(1)).set("role", "assistant"),
    )
    .await
    .expect("message");
    db
}

#[tokio::test]
async fn message_reactions_accept_only_like_and_dislike() {
    let db = db_with_message().await;
    db.insert("message_reactions", reaction_row(id(70), id(10), "like"))
        .await
        .expect("like");
    db.insert(
        "message_reactions",
        reaction_row(id(71), id(10), "dislike").set("user_id", id(0x56)),
    )
    .await
    .expect("dislike");

    let bad = db
        .insert(
            "message_reactions",
            reaction_row(id(72), id(10), "love").set("user_id", id(0x57)),
        )
        .await;

    assert!(bad.is_err(), "CHECK reaction IN ('like','dislike')");
}

#[tokio::test]
async fn message_reactions_allow_one_reaction_per_user_and_message() {
    let db = db_with_message().await;
    db.insert("message_reactions", reaction_row(id(70), id(10), "like"))
        .await
        .expect("first reaction");

    let second = db
        .insert("message_reactions", reaction_row(id(71), id(10), "dislike"))
        .await;

    assert!(second.is_err(), "UNIQUE (message_id, user_id)");
}

#[tokio::test]
async fn message_reactions_require_an_existing_message() {
    let db = db_with_chat().await;

    let result = db
        .insert("message_reactions", reaction_row(id(70), id(999), "like"))
        .await;

    assert!(result.is_err(), "FK message_id -> messages(id)");
}

// ---- cascades ----------------------------------------------------------------

#[tokio::test]
async fn deleting_a_chat_cascades_to_every_child_table() {
    let db = db_with_message_and_attachment().await;
    db.insert(
        "chat_turns",
        turn_row(id(20), tenant(), id(1), id(500), "completed"),
    )
    .await
    .expect("turn");
    db.insert("thread_summaries", summary_row(id(40), id(1)))
        .await
        .expect("summary");
    db.insert(
        "message_attachments",
        message_attachment_row(id(1), id(10), id(30)),
    )
    .await
    .expect("link");
    db.insert("message_reactions", reaction_row(id(70), id(10), "like"))
        .await
        .expect("reaction");

    db.delete_by_id("chats", id(1)).await.expect("delete chat");

    for table in [
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "message_reactions",
    ] {
        assert_eq!(db.count(table).await, 0, "`{table}` rows must cascade");
    }
}
