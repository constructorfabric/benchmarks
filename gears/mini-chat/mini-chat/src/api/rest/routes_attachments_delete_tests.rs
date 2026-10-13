#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: `DELETE /v1/chats/{id}/attachments/{attachment_id}`
//! (`cpt-cf-mini-chat-flow-attachments-delete-attachment`,
//! `cpt-cf-mini-chat-dod-attachments-delete`): soft delete with one cleanup
//! message in the same transaction, idempotence, the lock, the uploader check,
//! `secondary_ref`, and the failures that leave the row unchanged.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use serde_json::json;
use tracing::Level;
use uuid::Uuid;

use crate::attachment_test_env::{ANTHROPIC_ALIAS, ATTACHMENT_TYPE, AttEnv, AttOptions};
use crate::chat_test_env::{CHAT_PEP_TYPE, CHAT_TYPE, blob, ts};
use crate::domain::service::attachments::test_support::Metric;
use crate::domain::service::quota::test_support::PolicyStep;
use crate::test_doubles::{
    LogCapture, OTHER_USER, PdpReply, USER_TENANT, other_tenant_ctx, other_user_ctx, user_ctx,
};

async fn setup() -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    (env, chat)
}

const TEN_KEYS: [&str; 10] = [
    "event_type",
    "tenant_id",
    "chat_id",
    "attachment_id",
    "provider_file_id",
    "vector_store_id",
    "storage_backend",
    "attachment_kind",
    "deleted_at",
    "secondary_ref",
];

// ================================================================================================
// The delete
// ================================================================================================

#[tokio::test]
async fn deleting_an_unreferenced_attachment_is_204_and_enqueues_one_cleanup_message() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty(), "no body");
    let row = env.att(id).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.status, "ready");
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    let keys: BTreeSet<&str> = messages[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, TEN_KEYS.iter().copied().collect());
    assert_eq!(messages[0]["event_type"], "attachment_deleted");
    assert_eq!(messages[0]["tenant_id"], USER_TENANT.to_string());
    assert_eq!(messages[0]["chat_id"], chat.to_string());
    assert_eq!(messages[0]["attachment_id"], id.to_string());
    assert_eq!(messages[0]["provider_file_id"], "file-prov1");
    assert_eq!(messages[0]["vector_store_id"], json!(null));
    assert_eq!(messages[0]["storage_backend"], "openai");
    assert_eq!(messages[0]["attachment_kind"], "document");
    assert_eq!(messages[0]["secondary_ref"], json!(null));
    assert_eq!(messages[0]["deleted_at"], row.deleted_at.clone().unwrap());
    assert_eq!(
        env.cleanup_partitions().await,
        vec![i64::from(env.partition_of(USER_TENANT, chat))],
        "partitioned by tenant"
    );
}

#[tokio::test]
async fn the_response_never_waits_for_the_provider() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "no provider call, no Vector Stores API call"
    );
}

#[tokio::test]
async fn a_deleted_attachment_disappears_from_get() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    env.delete_attachment(chat, id).await;

    let reply = env.get_attachment(chat, id).await;

    reply.assert_not_found(ATTACHMENT_TYPE);
}

#[tokio::test]
async fn a_repeated_delete_is_204_without_a_second_message_or_any_write() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    env.delete_attachment(chat, id).await;
    let before = env.attachments_dump().await;
    env.advance(60);

    let again = env.delete_attachment(chat, id).await;

    assert_eq!(again.status, StatusCode::NO_CONTENT, "{}", again.text);
    assert_eq!(env.cleanup_messages().await.len(), 1);
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn a_pending_row_that_never_reached_the_provider_has_a_null_file_in_its_message() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        env.cleanup_messages().await[0]["provider_file_id"],
        json!(null)
    );
}

#[tokio::test]
async fn an_image_message_names_its_kind() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("attachment_kind", "image")
                .set("for_file_search", false)
        })
        .await;

    env.delete_attachment(chat, id).await;

    assert_eq!(env.cleanup_messages().await[0]["attachment_kind"], "image");
}

#[tokio::test]
async fn a_row_that_already_has_a_cleanup_status_is_deleted_without_a_new_message() {
    let (env, chat) = setup().await;
    let mut ids = Vec::new();
    for (n, state) in [(1, "pending"), (2, "done"), (3, "failed")] {
        ids.push((
            state,
            env.seed(chat, n, |row| row.set("cleanup_status", state))
                .await,
        ));
    }

    for (state, id) in ids {
        let reply = env.delete_attachment(chat, id).await;

        assert_eq!(
            reply.status,
            StatusCode::NO_CONTENT,
            "{state}: {}",
            reply.text
        );
        let row = env.att(id).await;
        assert!(row.deleted_at.is_some());
        assert_eq!(row.cleanup_status.as_deref(), Some(state));
    }
    assert!(env.cleanup_messages().await.is_empty());
}

#[tokio::test]
async fn concurrent_deletes_are_all_204_and_enqueue_one_message() {
    let env = Arc::new(
        AttEnv::with(AttOptions {
            file_db: true,
            ..AttOptions::default()
        })
        .await,
    );
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    let replies = futures::future::join_all((0..4).map(|_| env.delete_attachment(chat, id))).await;

    for reply in &replies {
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    }
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

// ================================================================================================
// The lock
// ================================================================================================

#[tokio::test]
async fn an_attachment_referenced_by_a_message_is_409_attachment_locked_and_unchanged() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    env.link(chat, id, 1).await;
    let before = env.attachments_dump().await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert!(
        reply.category().contains("already_exists"),
        "{}",
        reply.text
    );
    assert_eq!(reply.json["context"]["resource_name"], "attachment_locked");
    assert_eq!(reply.json["context"]["resource_type"], ATTACHMENT_TYPE);
    assert_eq!(env.attachments_dump().await, before);
    assert!(env.cleanup_messages().await.is_empty());
}

#[tokio::test]
async fn a_link_to_a_soft_deleted_message_still_locks_the_attachment() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    env.link(chat, id, 1).await;
    env.stream
        .quota
        .db
        .soft_delete("messages", Uuid::from_u128(0xD100_0001))
        .await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert_eq!(reply.json["context"]["resource_name"], "attachment_locked");
}

// ================================================================================================
// 404, authorization
// ================================================================================================

#[tokio::test]
async fn an_attachment_that_is_not_the_callers_is_404_with_the_attachment_type() {
    let (env, chat) = setup().await;
    let other_chat = env.chat("m-oa").await;
    let in_other_chat = env.seed(other_chat, 1, |row| row).await;
    let other_uploader = env
        .seed(chat, 2, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;

    for (label, id) in [
        ("unknown", Uuid::new_v4()),
        ("of another chat", in_other_chat),
        ("uploaded by another user", other_uploader),
    ] {
        let reply = env.delete_attachment(chat, id).await;

        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{label}: {}",
            reply.text
        );
        reply.assert_not_found(ATTACHMENT_TYPE);
    }
    assert!(env.att(other_uploader).await.deleted_at.is_none());
    assert!(env.cleanup_messages().await.is_empty());
}

#[tokio::test]
async fn the_uploader_is_checked_before_the_idempotent_repeat() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("uploaded_by_user_id", OTHER_USER)
                .set("deleted_at", ts(5))
                .set("cleanup_status", "done")
        })
        .await;

    let reply = env.delete_attachment(chat, id).await;

    reply.assert_not_found(ATTACHMENT_TYPE);
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_404_with_the_chat_type() {
    let env = AttEnv::new().await;
    let own = env.chat("m-oa").await;
    let id = env.seed(own, 1, |row| row).await;
    let foreign = env.chat_of(USER_TENANT, OTHER_USER, "m-oa").await;
    let in_foreign = env
        .seed(foreign, 2, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;

    for (label, ctx, chat, attachment) in [
        ("unknown", user_ctx(), Uuid::new_v4(), id),
        ("another user's chat", user_ctx(), foreign, in_foreign),
        ("mine, asked by another user", other_user_ctx(), own, id),
        ("mine, asked by another tenant", other_tenant_ctx(), own, id),
    ] {
        let reply = env.delete_attachment_as(&ctx, chat, attachment).await;

        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{label}: {}",
            reply.text
        );
        reply.assert_not_found(CHAT_TYPE);
    }
    assert!(env.att(id).await.deleted_at.is_none());
}

#[tokio::test]
async fn path_ids_that_are_not_uuids_are_400_invalid_path_params() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    for uri in [
        format!("/mini-chat/v1/chats/nope/attachments/{id}"),
        format!("/mini-chat/v1/chats/{chat}/attachments/nope"),
    ] {
        let reply = env
            .send("DELETE", &uri, &user_ctx(), None, Body::empty())
            .await;

        reply.assert_invalid("path", "invalid_path_params");
    }
}

#[tokio::test]
async fn the_delete_asks_the_pdp_for_delete_attachment_on_the_chat() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    env.delete_attachment(chat, id).await;

    let requests = env.stream.quota.pdp.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].action.name, "delete_attachment");
    assert_eq!(requests[0].resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(requests[0].resource.id, Some(chat));
    assert!(requests[0].context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_or_failure_changes_nothing() {
    for (pdp, status) in [(PdpReply::Deny, 403_u16), (PdpReply::Failure, 503)] {
        let env = AttEnv::with(AttOptions {
            pdp,
            ..AttOptions::default()
        })
        .await;
        let chat = env.chat("m-oa").await;
        let id = env.seed(chat, 1, |row| row).await;
        let before = env.attachments_dump().await;

        let reply = env.delete_attachment(chat, id).await;

        assert_eq!(reply.status.as_u16(), status, "{}", reply.text);
        if status == 503 {
            assert_eq!(reply.header("retry-after"), Some("5"));
        } else {
            assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");
        }
        assert_eq!(env.attachments_dump().await, before);
        assert!(env.cleanup_messages().await.is_empty());
    }
}

#[tokio::test]
async fn a_pdp_that_scopes_by_the_chat_id_still_lets_the_owner_delete() {
    let env = AttEnv::with(AttOptions {
        pdp: PdpReply::AllowChatConstraints,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
}

// ================================================================================================
// secondary_ref
// ================================================================================================

async fn anthropic_image(env: &AttEnv, chat: Uuid, n: u128, secondary: &str) -> Uuid {
    env.seed(chat, n, |row| {
        row.set("attachment_kind", "image")
            .set("for_file_search", false)
            .set("secondary_status", secondary)
            .set("secondary_provider_kind", "anthropic")
            .set("secondary_file_id", format!("file_ant_{n}"))
    })
    .await
}

#[tokio::test]
async fn an_uploaded_secondary_copy_is_referenced_with_the_anthropic_alias() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    let id = anthropic_image(&env, chat, 1, "uploaded").await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(
        env.cleanup_messages().await[0]["secondary_ref"],
        json!({ "file_id": "file_ant_1", "provider_kind": "anthropic", "upstream_alias": ANTHROPIC_ALIAS })
    );
    assert_eq!(env.metrics.count(&Metric::SecondarySkipped), 0);
}

#[tokio::test]
async fn a_secondary_copy_that_is_not_uploaded_is_not_referenced() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    for (n, state) in [(1, "failed"), (2, "pending"), (3, "not_attempted")] {
        let id = anthropic_image(&env, chat, n, state).await;

        env.delete_attachment(chat, id).await;
    }

    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 3, "each delete enqueued its message");
    for message in messages {
        assert_eq!(message["secondary_ref"], json!(null));
    }
    assert_eq!(
        env.metrics.count(&Metric::SecondarySkipped),
        0,
        "nothing was skipped: there was no copy"
    );
}

#[tokio::test]
async fn an_unresolvable_alias_gives_a_null_reference_a_warning_and_a_skipped_metric() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    let id = anthropic_image(&env, chat, 1, "uploaded").await;
    env.stream.quota.policy.fail(PolicyStep::Snapshot);
    let logs = LogCapture::start();

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(
        reply.status,
        StatusCode::NO_CONTENT,
        "the delete itself still succeeds: {}",
        reply.text
    );
    assert_eq!(
        env.cleanup_messages().await[0]["secondary_ref"],
        json!(null)
    );
    assert_eq!(env.metrics.count(&Metric::SecondarySkipped), 1);
    assert!(
        !logs.at_containing(Level::WARN, &id.to_string()).is_empty(),
        "a warning names the attachment: {}",
        logs.all_text()
    );
}

#[tokio::test]
async fn a_chat_whose_provider_is_not_anthropic_has_no_secondary_reference() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = anthropic_image(&env, chat, 1, "uploaded").await;

    env.delete_attachment(chat, id).await;

    assert_eq!(
        env.cleanup_messages().await[0]["secondary_ref"],
        json!(null)
    );
    assert_eq!(env.metrics.count(&Metric::SecondarySkipped), 1);
}

// ================================================================================================
// The payload is built inside the transaction
// ================================================================================================

#[tokio::test]
async fn a_provider_file_stored_between_the_read_and_the_transaction_is_not_lost() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await;
    // The delete flow reads the row `pending` without a file; the upload stores
    // its file id before the delete transaction opens.
    let raw = env.stream.quota.db.raw.clone();
    let sql = format!(
        "UPDATE attachments SET status = 'uploaded', provider_file_id = 'file-late' WHERE id = {}",
        blob(id)
    );
    env.repo.after("find", move || {
        let raw = raw.clone();
        let sql = sql.clone();
        Box::pin(async move {
            use sea_orm::ConnectionTrait;
            raw.execute_unprepared(&sql).await.unwrap();
        })
    });

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]["provider_file_id"], "file-late",
        "the payload comes from the row read in the transaction"
    );
}

// ================================================================================================
// Failures leave the row unchanged
// ================================================================================================

#[tokio::test]
async fn a_failed_enqueue_is_500_and_the_row_is_unchanged() {
    let env = AttEnv::with(AttOptions {
        enqueue_fails: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    let before = env.attachments_dump().await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(reply.category().contains("internal"));
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn an_oversized_payload_is_500_and_the_row_is_unchanged() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("provider_file_id", "f".repeat(70_000))
        })
        .await;
    let before = env.attachments_dump().await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn a_database_failure_is_500_with_a_generic_detail() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    env.fail_updates("no_delete", "NEW.deleted_at IS NOT NULL")
        .await;
    let before = env.attachments_dump().await;

    let reply = env.delete_attachment(chat, id).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(!reply.text.contains("injected"), "{}", reply.text);
    assert_eq!(env.attachments_dump().await, before);
}
