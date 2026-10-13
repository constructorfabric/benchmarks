#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The database schema of DESIGN section 3.7 on SQLite: every table exists, the enforced CHECKs,
//! UNIQUEs, partial indexes and foreign keys behave as specified, and UUIDs are stored as 16-byte
//! blobs.

mod common;

use common::{chat_id, create_default_chat};
use mini_chat::test_support::{TestEnv, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use uuid::Uuid;

const TS: &str = "'2026-01-01T00:00:00+00:00'";

async fn run(env: &TestEnv, sql: &str) -> Result<(), sea_orm::DbErr> {
    env.db()
        .execute_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
        .await
        .map(|_| ())
}

async fn count(env: &TestEnv, sql: &str) -> i64 {
    let row = env
        .db()
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
        .await
        .expect("query must succeed")
        .expect("COUNT always returns a row");
    row.try_get("", "n").unwrap()
}

fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

struct Fixture {
    env: TestEnv,
    tenant: Uuid,
    user: Uuid,
    chat: Uuid,
}

async fn fixture() -> Fixture {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = Uuid::parse_str(&chat_id(&create_default_chat(&env, &user).await)).unwrap();
    Fixture {
        env,
        tenant: user.tenant_id,
        user: user.user_id,
        chat,
    }
}

impl Fixture {
    async fn message(&self, request: Option<Uuid>, role: &str) -> Result<Uuid, sea_orm::DbErr> {
        let id = Uuid::new_v4();
        let request = request.map_or_else(|| "NULL".to_owned(), blob);
        run(
            &self.env,
            &format!(
                "INSERT INTO messages (id, tenant_id, chat_id, request_id, role, content, created_at) \
                 VALUES ({}, {}, {}, {request}, '{role}', 'hi', {TS})",
                blob(id),
                blob(self.tenant),
                blob(self.chat),
            ),
        )
        .await
        .map(|()| id)
    }

    async fn turn(&self, state: &str, deleted: bool) -> Result<(), sea_orm::DbErr> {
        let deleted_at = if deleted { TS } else { "NULL" };
        run(
            &self.env,
            &format!(
                "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, \
                 deleted_at, started_at, updated_at) VALUES ({}, {}, {}, {}, 'user', '{state}', \
                 {deleted_at}, {TS}, {TS})",
                blob(Uuid::new_v4()),
                blob(self.tenant),
                blob(self.chat),
                blob(Uuid::new_v4()),
            ),
        )
        .await
    }

    async fn attachment(&self, status: &str, kind: &str) -> Result<Uuid, sea_orm::DbErr> {
        let id = Uuid::new_v4();
        run(
            &self.env,
            &format!(
                "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
                 status, attachment_kind, created_at, updated_at) VALUES ({}, {}, {}, {}, 'f.txt', \
                 '{status}', '{kind}', {TS}, {TS})",
                blob(id),
                blob(self.tenant),
                blob(self.chat),
                blob(self.user),
            ),
        )
        .await
        .map(|()| id)
    }
}

#[tokio::test]
async fn every_table_of_the_design_and_the_outbox_exists() {
    let env = TestEnv::new().await;

    for table in [
        "chats",
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
        "message_reactions",
        "mini_chat__outbox_body",
        "mini_chat__outbox_partitions",
        "mini_chat__outbox_incoming",
        "mini_chat__outbox_outgoing",
        "mini_chat__outbox_dead_letters",
    ] {
        let found = count(
            &env,
            &format!(
                "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
            ),
        )
        .await;
        assert_eq!(found, 1, "table `{table}` must exist");
    }
}

#[tokio::test]
async fn the_documented_indexes_exist() {
    let env = TestEnv::new().await;

    for index in [
        "idx_chats_owner_updated",
        "uq_messages_request_role",
        "idx_messages_chat_created",
        "idx_chat_turns_chat_started",
        "uq_chat_turns_one_running",
        "idx_chat_turns_orphan_scan",
        "idx_attachments_tenant_chat",
        "idx_attachments_cleanup_status",
        "idx_attachments_stale_upload",
        "idx_message_attachments_tenant_chat",
        "idx_message_attachments_attachment_chat",
    ] {
        let found = count(
            &env,
            &format!(
                "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'index' AND name = '{index}'"
            ),
        )
        .await;
        assert_eq!(found, 1, "index `{index}` must exist");
    }
}

#[tokio::test]
async fn uuids_are_stored_as_sixteen_byte_blobs() {
    let fixture = fixture().await;

    let blobs = count(
        &fixture.env,
        "SELECT COUNT(*) AS n FROM chats WHERE typeof(id) = 'blob' AND length(id) = 16 \
         AND typeof(tenant_id) = 'blob' AND typeof(user_id) = 'blob'",
    )
    .await;

    assert_eq!(blobs, 1);
}

#[tokio::test]
async fn message_role_is_checked_and_a_request_has_one_message_per_role() {
    let fixture = fixture().await;
    let request = Uuid::new_v4();

    assert!(fixture.message(Some(request), "user").await.is_ok());
    assert!(fixture.message(Some(request), "assistant").await.is_ok());
    assert!(
        fixture.message(Some(request), "user").await.is_err(),
        "duplicate role per request"
    );
    assert!(fixture.message(None, "user").await.is_ok());
    assert!(
        fixture.message(None, "user").await.is_ok(),
        "no request id, no uniqueness"
    );
    assert!(fixture.message(None, "tool").await.is_err(), "unknown role");
}

#[tokio::test]
async fn a_chat_has_at_most_one_live_running_turn() {
    let fixture = fixture().await;

    assert!(fixture.turn("running", false).await.is_ok());
    assert!(
        fixture.turn("running", false).await.is_err(),
        "second running turn"
    );
    assert!(fixture.turn("completed", false).await.is_ok());
    assert!(
        fixture.turn("running", true).await.is_ok(),
        "deleted turns do not count"
    );
}

#[tokio::test]
async fn turn_state_and_requester_type_are_checked_but_cross_column_rules_are_not() {
    let fixture = fixture().await;

    assert!(fixture.turn("paused", false).await.is_err());
    // ADR-0010: a terminal turn without `completed_at` is not rejected by the database.
    assert!(fixture.turn("failed", false).await.is_ok());
    let bad_requester = run(
        &fixture.env,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, \
             started_at, updated_at) VALUES ({}, {}, {}, {}, 'robot', 'failed', {TS}, {TS})",
            blob(Uuid::new_v4()),
            blob(fixture.tenant),
            blob(fixture.chat),
            blob(Uuid::new_v4()),
        ),
    )
    .await;
    assert!(bad_requester.is_err());
}

#[tokio::test]
async fn attachment_status_and_kind_are_checked_and_cleanup_status_is_free_text() {
    let fixture = fixture().await;

    assert!(fixture.attachment("ready", "document").await.is_ok());
    assert!(fixture.attachment("pending", "image").await.is_ok());
    assert!(fixture.attachment("lost", "document").await.is_err());
    assert!(fixture.attachment("ready", "video").await.is_err());
    let id = fixture.attachment("ready", "document").await.unwrap();
    let free_text = run(
        &fixture.env,
        &format!(
            "UPDATE attachments SET cleanup_status = 'whatever' WHERE id = {}",
            blob(id)
        ),
    )
    .await;
    assert!(free_text.is_ok(), "ADR-0010: no CHECK on cleanup_status");
}

#[tokio::test]
async fn message_attachments_are_bound_to_one_chat_by_composite_foreign_keys() {
    let fixture = fixture().await;
    let other_chat = Uuid::parse_str(&chat_id(
        &create_default_chat(&fixture.env, &TestUser::in_tenant(fixture.tenant)).await,
    ))
    .unwrap();
    let message = fixture.message(None, "user").await.unwrap();
    let attachment = fixture.attachment("ready", "document").await.unwrap();
    let link = |chat: Uuid| {
        format!(
            "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, created_at) \
             VALUES ({}, {}, {}, {}, {TS})",
            blob(fixture.tenant),
            blob(chat),
            blob(message),
            blob(attachment),
        )
    };

    assert!(
        run(&fixture.env, &link(other_chat)).await.is_err(),
        "chat mismatch"
    );
    assert!(run(&fixture.env, &link(fixture.chat)).await.is_ok());
    assert!(
        run(&fixture.env, &link(fixture.chat)).await.is_err(),
        "duplicate link"
    );
}

#[tokio::test]
async fn children_cascade_with_their_chat_and_reactions_with_their_message() {
    let fixture = fixture().await;
    let message = fixture.message(None, "assistant").await.unwrap();
    fixture.turn("completed", false).await.unwrap();
    fixture.attachment("ready", "document").await.unwrap();
    run(
        &fixture.env,
        &format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, 'like', {TS})",
            blob(Uuid::new_v4()),
            blob(message),
            blob(fixture.user),
            blob(fixture.tenant),
        ),
    )
    .await
    .unwrap();

    run(
        &fixture.env,
        &format!("DELETE FROM chats WHERE id = {}", blob(fixture.chat)),
    )
    .await
    .unwrap();

    for table in ["messages", "chat_turns", "attachments", "message_reactions"] {
        let remaining = count(&fixture.env, &format!("SELECT COUNT(*) AS n FROM {table}")).await;
        assert_eq!(remaining, 0, "{table}");
    }
}

#[tokio::test]
async fn reaction_values_are_checked_and_one_reaction_per_user_and_message() {
    let fixture = fixture().await;
    let message = fixture.message(None, "assistant").await.unwrap();
    let reaction = |reaction: &str, user: Uuid| {
        format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, '{reaction}', {TS})",
            blob(Uuid::new_v4()),
            blob(message),
            blob(user),
            blob(fixture.tenant),
        )
    };

    assert!(
        run(&fixture.env, &reaction("love", fixture.user))
            .await
            .is_err()
    );
    assert!(
        run(&fixture.env, &reaction("like", fixture.user))
            .await
            .is_ok()
    );
    assert!(
        run(&fixture.env, &reaction("dislike", fixture.user))
            .await
            .is_err()
    );
    assert!(
        run(&fixture.env, &reaction("dislike", Uuid::new_v4()))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn summaries_vector_stores_and_quota_buckets_are_unique_per_their_keys() {
    let fixture = fixture().await;
    let summary = |chat: Uuid| {
        format!(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, \
             summarized_up_to_created_at, summarized_up_to_message_id, created_at, updated_at) \
             VALUES ({}, {}, {}, 's', {TS}, {}, {TS}, {TS})",
            blob(Uuid::new_v4()),
            blob(fixture.tenant),
            blob(chat),
            blob(Uuid::new_v4()),
        )
    };
    let store = |file_count: i64| {
        format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, provider, file_count, created_at) \
             VALUES ({}, {}, {}, 'azure', {file_count}, {TS})",
            blob(Uuid::new_v4()),
            blob(fixture.tenant),
            blob(fixture.chat),
        )
    };
    let quota = |bucket: &str, spent: i64| {
        format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
             spent_credits_micro, updated_at) VALUES ({}, {}, {}, 'daily', '2026-01-01', '{bucket}', \
             {spent}, {TS})",
            blob(Uuid::new_v4()),
            blob(fixture.tenant),
            blob(fixture.user),
        )
    };

    assert!(run(&fixture.env, &summary(fixture.chat)).await.is_ok());
    assert!(
        run(&fixture.env, &summary(fixture.chat)).await.is_err(),
        "one summary per chat"
    );
    assert!(
        run(&fixture.env, &summary(Uuid::new_v4())).await.is_err(),
        "summary needs a chat"
    );

    assert!(run(&fixture.env, &store(0)).await.is_ok());
    assert!(
        run(&fixture.env, &store(0)).await.is_err(),
        "one vector store per chat"
    );

    assert!(run(&fixture.env, &quota("total", 0)).await.is_ok());
    assert!(
        run(&fixture.env, &quota("total", 0)).await.is_err(),
        "one row per bucket"
    );
    assert!(run(&fixture.env, &quota("tier:premium", 0)).await.is_ok());
    assert!(
        run(&fixture.env, &quota("tier:standard", -1))
            .await
            .is_err(),
        "counters are non-negative"
    );
}
