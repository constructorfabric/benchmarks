//! `DELETE /v1/chats/{id}` over the real router, SQLite, the shared outbox and the mock PDP:
//! the soft-delete, the attachment marking and the chat-cleanup enqueue in one transaction, the
//! rollback paths and the Deleted state.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: flow
//! `cpt-cf-mini-chat-flow-chat-management-delete-chat`; algorithms `delete-transaction`,
//! `chat-cleanup-enqueue`, `activity-ordering` (delete), `chat-isolation`; state `chat-lifecycle`
//! (Active to Deleted, Deleted terminal); definition of done
//! `cpt-cf-mini-chat-dod-chat-management-delete`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::chats::ChatCleanupQueue;
use crate::infra::outbox::{CHAT_CLEANUP_PAYLOAD_TYPE, partition_for};
use crate::test_support::Pdp;
use crate::test_support::chats::{
    CHATS, ChatRig, FailingQueue, Queue, RigOptions, a1, a2, assert_chat_not_found, assert_problem,
    at, attachment_rows, b1, chat_row, chat_uri, children_counts, fail_attachment_updates,
    fail_outbox_inserts, item_ids, message_rows, outbox_body_count, parse_ts, rig, rig_with,
    seed_attachment, seed_message, seed_reaction, seed_running_turn,
};

fn close(left: OffsetDateTime, right: OffsetDateTime) -> bool {
    (left - right).abs() < time::Duration::milliseconds(1)
}

async fn failing_rig(queue: Queue) -> ChatRig {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.queue = queue;
    rig_with(options).await
}

/// Assert that the chat is still active, its attachments are unmarked and nothing was enqueued.
async fn assert_rolled_back(rig: &ChatRig, chat_id: Uuid, updated_at: OffsetDateTime) {
    let row = chat_row(&rig.db, chat_id).await.unwrap();
    assert_eq!(row.deleted_at, None, "the chat stays active");
    assert_eq!(row.updated_at, updated_at, "updated_at is not bumped");
    let attachments = attachment_rows(&rig.db, chat_id).await;
    assert!(!attachments.is_empty());
    assert!(
        attachments.iter().all(|a| a.cleanup_status.is_none()),
        "the attachments are not marked"
    );
    assert_eq!(outbox_body_count(&rig.db).await, 0, "no outbox message");
}

// ── success ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_returns_204_with_no_body_and_the_chat_disappears() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("doomed"), "gpt-standard").await;
    let other = rig.seed(&a1(), Some("survivor"), "gpt-standard").await;

    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty(), "no body: {}", reply.text);

    assert_chat_not_found(&rig.get(&chat_uri(chat.id), &a1()).await);
    let listed = rig.get(CHATS, &a1()).await;
    assert_eq!(item_ids(&listed.json()), vec![other.id]);
    assert_eq!(
        chat_row(&rig.db, other.id).await.unwrap().deleted_at,
        None,
        "other chats are untouched"
    );
}

#[tokio::test]
async fn the_row_is_kept_with_deleted_at_and_updated_at_set_to_the_same_time() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("doomed"), "gpt-standard").await;
    let started = OffsetDateTime::now_utc();
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    let row = chat_row(&rig.db, chat.id).await.expect("no hard purge");
    let deleted_at = row.deleted_at.expect("deleted_at is set");
    assert!(deleted_at >= started - time::Duration::seconds(1));
    assert_eq!(
        row.updated_at, deleted_at,
        "updated_at is the deletion time"
    );
    assert_eq!(row.title.as_deref(), Some("doomed"));
    assert_eq!(row.model, "gpt-standard");
}

#[tokio::test]
async fn the_attachments_are_marked_pending_and_existing_statuses_are_kept() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("with files"), "m").await;
    let sibling = rig.seed(&a1(), Some("sibling"), "m").await;
    let document = seed_attachment(&rig.db, &chat, "document", None).await;
    let image = seed_attachment(&rig.db, &chat, "image", None).await;
    let pending = seed_attachment(&rig.db, &chat, "document", Some("pending")).await;
    let done = seed_attachment(&rig.db, &chat, "document", Some("done")).await;
    let failed = seed_attachment(&rig.db, &chat, "image", Some("failed")).await;
    seed_attachment(&rig.db, &sibling, "document", None).await;

    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );

    let rows = attachment_rows(&rig.db, chat.id).await;
    let status = |id: Uuid| {
        rows.iter()
            .find(|r| r.id == id)
            .unwrap()
            .cleanup_status
            .clone()
    };
    assert_eq!(status(document).as_deref(), Some("pending"));
    assert_eq!(
        status(image).as_deref(),
        Some("pending"),
        "documents and images"
    );
    assert_eq!(status(pending).as_deref(), Some("pending"));
    assert_eq!(status(done).as_deref(), Some("done"));
    assert_eq!(status(failed).as_deref(), Some("failed"));
    assert!(rows.iter().all(|r| r.deleted_at.is_none()));
    let sibling_rows = attachment_rows(&rig.db, sibling.id).await;
    assert!(
        sibling_rows.iter().all(|r| r.cleanup_status.is_none()),
        "other chats are not marked"
    );
}

#[tokio::test]
async fn messages_turns_and_reactions_are_not_modified_and_a_running_turn_is_not_cancelled() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("busy"), "m").await;
    seed_message(&rig.db, &chat, "user").await;
    let assistant = seed_message(&rig.db, &chat, "assistant").await;
    seed_reaction(&rig.db, &chat, assistant).await;
    let turn = seed_running_turn(&rig.db, &chat).await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    let counts = children_counts(&rig.db, chat.id).await;
    let messages = message_rows(&rig.db, chat.id).await;
    assert_eq!(counts, (2, 1, 1, 0));

    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );

    assert_eq!(children_counts(&rig.db, chat.id).await, counts);
    assert_eq!(message_rows(&rig.db, chat.id).await, messages);
    let state = crate::test_support::raw_i64(
        &rig.db.raw,
        &format!(
            "SELECT COUNT(*) FROM chat_turns WHERE id = {} AND state = 'running' \
             AND deleted_at IS NULL",
            crate::test_support::chats::blob(turn)
        ),
    )
    .await;
    assert_eq!(
        state, 1,
        "the running turn is neither cancelled nor deleted"
    );
}

// ── the chat-cleanup message ─────────────────────────────────────────────────

#[tokio::test]
async fn exactly_one_chat_cleanup_message_is_enqueued_with_the_design_payload() {
    let rig = rig(Pdp::TenantOnly).await;
    // tail 0x0002: partition 2, while the tenant routes to partition 1
    let chat_id = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0002);
    let chat = rig
        .seed_at(&a1(), chat_id, Some("doomed"), "m", at(0))
        .await;
    seed_attachment(&rig.db, &chat, "document", None).await;

    assert_eq!(
        rig.delete(&chat_uri(chat_id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );

    let rows = rig.outbox_rows().await;
    assert_eq!(
        rows.len(),
        1,
        "one message per deletion, nothing else (no audit event)"
    );
    let row = &rows[0];
    assert_eq!(row.queue, "mini-chat.chat_cleanup");
    assert_eq!(row.queue, rig.outbox.chat_cleanup_queue_name);
    assert_eq!(row.payload_type, CHAT_CLEANUP_PAYLOAD_TYPE);
    assert_eq!(
        row.partition,
        i64::from(partition_for(chat_id, rig.outbox.num_partitions)),
        "partitioned by chat_id"
    );

    let payload = &row.payload;
    assert_eq!(
        Uuid::parse_str(payload["tenant_id"].as_str().unwrap()).unwrap(),
        chat.tenant_id
    );
    assert_eq!(
        Uuid::parse_str(payload["chat_id"].as_str().unwrap()).unwrap(),
        chat_id
    );
    assert_eq!(payload["reason"], "chat_soft_delete");
    let request_id = Uuid::parse_str(payload["system_request_id"].as_str().unwrap()).unwrap();
    assert_eq!(request_id.get_version_num(), 4);
    let deleted_at = chat_row(&rig.db, chat_id)
        .await
        .unwrap()
        .deleted_at
        .unwrap();
    assert!(
        close(parse_ts(&payload["chat_deleted_at"]), deleted_at),
        "chat_deleted_at is the deleted_at the soft-delete wrote: {payload}"
    );
}

#[tokio::test]
async fn a_chat_without_attachments_is_cleaned_up_through_the_same_message() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("bare"), "m").await;
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(rig.cleanup_rows().await.len(), 1);
}

#[tokio::test]
async fn the_message_goes_to_the_configured_queue() {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.outbox = crate::config::OutboxConfig {
        chat_cleanup_queue_name: "acme.chat_cleanup".to_owned(),
        ..crate::config::OutboxConfig::default()
    };
    let rig = rig_with(options).await;
    let chat = rig.seed(&a1(), Some("doomed"), "m").await;
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, "acme.chat_cleanup");
}

#[tokio::test]
async fn deleting_two_chats_enqueues_two_messages_with_distinct_request_ids() {
    let rig = rig(Pdp::TenantOnly).await;
    let first = rig.seed(&a1(), Some("one"), "m").await;
    let second = rig.seed(&a1(), Some("two"), "m").await;
    for chat in [&first, &second] {
        assert_eq!(
            rig.delete(&chat_uri(chat.id), &a1()).await.status,
            StatusCode::NO_CONTENT
        );
    }
    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 2);
    assert_ne!(
        rows[0].payload["system_request_id"],
        rows[1].payload["system_request_id"]
    );
}

// ── the Deleted state ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_delete_is_a_404_and_enqueues_nothing_more() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("doomed"), "m").await;
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    let first_deleted_at = chat_row(&rig.db, chat.id).await.unwrap().deleted_at;
    assert_eq!(rig.outbox_rows().await.len(), 1);

    let second = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_chat_not_found(&second);
    assert_eq!(
        rig.outbox_rows().await.len(),
        1,
        "nothing is enqueued by the second delete"
    );
    assert_eq!(
        chat_row(&rig.db, chat.id).await.unwrap().deleted_at,
        first_deleted_at,
        "nothing changes"
    );
}

#[tokio::test]
async fn every_operation_on_a_deleted_chat_is_a_404_and_changes_nothing() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("doomed"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    let row = chat_row(&rig.db, chat.id).await.unwrap();
    let attachments = attachment_rows(&rig.db, chat.id).await;

    assert_chat_not_found(&rig.get(&chat_uri(chat.id), &a1()).await);
    assert_chat_not_found(
        &rig.patch_json(&chat_uri(chat.id), &a1(), r#"{"title":"revived"}"#)
            .await,
    );
    assert_chat_not_found(&rig.delete(&chat_uri(chat.id), &a1()).await);

    assert_eq!(chat_row(&rig.db, chat.id).await.unwrap(), row);
    assert_eq!(attachment_rows(&rig.db, chat.id).await, attachments);
    assert_eq!(rig.outbox_rows().await.len(), 1);
    assert!(item_ids(&rig.get(CHATS, &a1()).await.json()).is_empty());
}

// ── admission and masking ────────────────────────────────────────────────────

#[tokio::test]
async fn delete_is_authorized_as_delete_with_the_chat_id_and_required_constraints() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("t"), "m").await;
    assert_eq!(
        rig.delete(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "delete");
    assert_eq!(request.resource.id, Some(chat.id));
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_or_failure_refuses_the_delete_and_changes_nothing() {
    for (behavior, status) in [
        (Pdp::Deny, 403),
        (Pdp::NoConstraints, 403),
        (Pdp::Unreachable, 503),
    ] {
        let rig = rig(behavior).await;
        let chat = rig.seed(&a1(), Some("keep"), "m").await;
        seed_attachment(&rig.db, &chat, "document", None).await;
        let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
        let category = if status == 403 {
            "permission_denied"
        } else {
            "service_unavailable"
        };
        assert_problem(&reply, status, category);
        assert_rolled_back(&rig, chat.id, at(0)).await;
    }
}

#[tokio::test]
async fn a_foreign_or_missing_chat_is_a_404_and_nothing_is_marked_or_enqueued() {
    let rig = rig(Pdp::TenantOnly).await;
    let mine = rig.seed(&a1(), Some("mine"), "m").await;
    seed_attachment(&rig.db, &mine, "document", None).await;
    for (who, label) in [(a2(), "other user"), (b1(), "other tenant")] {
        let reply = rig.delete(&chat_uri(mine.id), &who).await;
        assert_chat_not_found(&reply);
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{label}: never 403");
    }
    assert_chat_not_found(&rig.delete(&chat_uri(Uuid::new_v4()), &a1()).await);
    assert_rolled_back(&rig, mine.id, at(0)).await;
}

#[tokio::test]
async fn a_non_uuid_path_id_is_a_400_invalid_path_params() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig.delete(&format!("{CHATS}/not-a-uuid"), &a1()).await;
    let body = assert_problem(&reply, 400, "invalid_argument");
    assert!(
        body["context"]["field_violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["reason"] == "invalid_path_params"),
        "{body}"
    );
    assert!(rig.pdp.requests().is_empty());
}

// ── atomicity ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_oversized_payload_is_a_400_and_the_whole_transaction_rolls_back() {
    let queue = FailingQueue::payload_too_large();
    let dyn_queue: Arc<dyn ChatCleanupQueue> = queue.clone();
    let rig = failing_rig(Queue::Custom(dyn_queue)).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    seed_attachment(&rig.db, &chat, "image", None).await;

    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    let body = assert_problem(&reply, 400, "invalid_argument");
    let detail = body["detail"].as_str().unwrap();
    assert!(!detail.is_empty());
    assert_eq!(
        body["context"]["format"], detail,
        "the message is also in context.format"
    );

    assert_eq!(queue.call_count(), 1, "the enqueue was reached once");
    assert_rolled_back(&rig, chat.id, at(0)).await;
    // and the chat is still fully usable
    assert_eq!(
        rig.get(&chat_uri(chat.id), &a1()).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_enqueue_failure_is_a_generic_500_and_leaves_the_chat_active_and_unmarked() {
    let queue = FailingQueue::internal();
    let dyn_queue: Arc<dyn ChatCleanupQueue> = queue.clone();
    let rig = failing_rig(Queue::Custom(dyn_queue)).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;

    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_problem(&reply, 500, "internal");
    assert!(
        !reply.text.contains("secret-queue-detail"),
        "cause leaked: {}",
        reply.text
    );
    assert_rolled_back(&rig, chat.id, at(0)).await;
}

#[tokio::test]
async fn an_outbox_that_is_not_running_is_a_500_and_nothing_is_deleted() {
    let rig = failing_rig(Queue::Unbound).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_problem(&reply, 500, "internal");
    assert_rolled_back(&rig, chat.id, at(0)).await;
}

#[tokio::test]
async fn a_database_failure_while_marking_the_attachments_rolls_back_the_soft_delete() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    fail_attachment_updates(&rig.db).await;

    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_problem(&reply, 500, "internal");
    assert!(
        !reply.text.contains("forced attachment failure"),
        "{}",
        reply.text
    );
    assert_rolled_back(&rig, chat.id, at(0)).await;
}

#[tokio::test]
async fn a_database_failure_while_enqueueing_rolls_back_the_soft_delete_and_the_marking() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    seed_attachment(&rig.db, &chat, "document", None).await;
    seed_attachment(&rig.db, &chat, "image", None).await;
    fail_outbox_inserts(&rig.db).await;

    let reply = rig.delete(&chat_uri(chat.id), &a1()).await;
    assert_problem(&reply, 500, "internal");
    assert!(
        !reply.text.contains("forced outbox failure"),
        "{}",
        reply.text
    );
    assert_rolled_back(&rig, chat.id, at(0)).await;
}
