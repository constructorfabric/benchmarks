#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the soft delete transaction
//! (`cpt-cf-mini-chat-algo-attachments-soft-delete`): write-first, the lock rule,
//! the cleanup payload built inside the transaction from the row read there.

use serde_json::json;
use uuid::Uuid;

use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::service::attachments::model::{DeleteOutcome, SoftDelete};
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::test_doubles::{OTHER_TENANT, USER_TENANT};

fn request(env: &AttEnv, chat: Uuid, id: Uuid, alias: Option<&str>) -> SoftDelete {
    SoftDelete {
        chat_id: chat,
        attachment_id: id,
        secondary_alias: alias.map(str::to_owned),
        now: env.now(),
    }
}

async fn delete(
    env: &AttEnv,
    chat: Uuid,
    id: Uuid,
    alias: Option<&str>,
) -> Result<DeleteOutcome, DomainError> {
    env.sea
        .soft_delete(&env.scope(), &request(env, chat, id, alias))
        .await
}

fn expected(
    env: &AttEnv,
    chat: Uuid,
    id: Uuid,
    file: Option<&str>,
    kind: &str,
) -> serde_json::Value {
    json!({
        "event_type": "attachment_deleted",
        "tenant_id": USER_TENANT.to_string(),
        "chat_id": chat.to_string(),
        "attachment_id": id.to_string(),
        "provider_file_id": file,
        "vector_store_id": null,
        "storage_backend": "openai",
        "attachment_kind": kind,
        "deleted_at": AttEnv::stamp(env.now()),
        "secondary_ref": null,
    })
}

#[tokio::test]
async fn the_delete_marks_the_row_and_enqueues_one_message_in_the_same_transaction() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| row.set("storage_backend", "openai"))
        .await;

    let outcome = delete(&env, chat, id, None).await.unwrap();

    let snap = env.att(id).await;
    let stamp = AttEnv::stamp(env.now());
    assert_eq!(outcome, DeleteOutcome::Deleted);
    assert_eq!(snap.deleted_at.as_deref(), Some(stamp.as_str()));
    assert_eq!(snap.updated_at, stamp);
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(snap.cleanup_updated_at.as_deref(), Some(stamp.as_str()));
    assert_eq!(snap.status, "ready", "status keeps its value");
    assert_eq!(
        env.cleanup_messages().await,
        vec![expected(&env, chat, id, Some("file-prov1"), "document")]
    );
    assert_eq!(
        env.cleanup_partitions().await,
        vec![i64::from(env.partition_of(USER_TENANT, chat))]
    );
}

#[tokio::test]
async fn the_payload_follows_the_row_read_in_the_transaction() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let pending = env
        .seed(chat, 1, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await;
    let image = env
        .seed(chat, 2, |row| {
            row.set("attachment_kind", "image")
                .set("for_file_search", false)
                .set("storage_backend", "openai")
        })
        .await;

    delete(&env, chat, pending, None).await.unwrap();
    delete(&env, chat, image, None).await.unwrap();

    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], expected(&env, chat, pending, None, "document"));
    assert_eq!(
        messages[1],
        expected(&env, chat, image, Some("file-prov2"), "image")
    );
}

#[tokio::test]
async fn a_secondary_copy_is_referenced_only_when_uploaded_and_the_alias_is_known() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-an").await;
    let uploaded = env
        .seed(chat, 1, |row| {
            row.set("attachment_kind", "image")
                .set("secondary_status", "uploaded")
                .set("secondary_file_id", "file_ant_1")
                .set("secondary_provider_kind", "anthropic")
        })
        .await;
    let no_alias = env
        .seed(chat, 2, |row| {
            row.set("secondary_status", "uploaded")
                .set("secondary_file_id", "file_ant_2")
        })
        .await;
    let failed = env
        .seed(chat, 3, |row| row.set("secondary_status", "failed"))
        .await;
    let pending = env
        .seed(chat, 4, |row| row.set("secondary_status", "pending"))
        .await;

    delete(&env, chat, uploaded, Some("api.anthropic.test"))
        .await
        .unwrap();
    delete(&env, chat, no_alias, None).await.unwrap();
    delete(&env, chat, failed, Some("api.anthropic.test"))
        .await
        .unwrap();
    delete(&env, chat, pending, Some("api.anthropic.test"))
        .await
        .unwrap();

    let refs: Vec<_> = env
        .cleanup_messages()
        .await
        .iter()
        .map(|m| m["secondary_ref"].clone())
        .collect();
    assert_eq!(
        refs,
        vec![
            json!({ "file_id": "file_ant_1", "provider_kind": "anthropic", "upstream_alias": "api.anthropic.test" }),
            json!(null),
            json!(null),
            json!(null),
        ]
    );
}

#[tokio::test]
async fn a_row_with_a_cleanup_status_is_deleted_without_a_new_message() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let mut ids = Vec::new();
    for (n, state) in [(1, "pending"), (2, "done"), (3, "failed")] {
        ids.push(
            env.seed(chat, n, |row| row.set("cleanup_status", state))
                .await,
        );
    }

    for (id, state) in ids.iter().zip(["pending", "done", "failed"]) {
        let outcome = delete(&env, chat, *id, None).await.unwrap();

        let snap = env.att(*id).await;
        assert_eq!(outcome, DeleteOutcome::Deleted);
        assert!(snap.deleted_at.is_some(), "{state}");
        assert_eq!(
            snap.cleanup_status.as_deref(),
            Some(state),
            "cleanup_status keeps its state"
        );
        assert_eq!(snap.cleanup_updated_at, None, "no cleanup write");
    }
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn deleting_an_already_deleted_row_writes_nothing() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    delete(&env, chat, id, None).await.unwrap();
    let before = env.attachments_dump().await;
    env.advance(30);

    let again = delete(&env, chat, id, None).await.unwrap();

    assert_eq!(again, DeleteOutcome::AlreadyDeleted);
    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

#[tokio::test]
async fn a_referenced_attachment_is_locked_and_the_row_is_unchanged() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    env.link(chat, id, 1).await;
    let before = env.attachments_dump().await;

    let outcome = delete(&env, chat, id, None).await.unwrap();

    assert_eq!(outcome, DeleteOutcome::Locked);
    assert_eq!(
        env.attachments_dump().await,
        before,
        "the write is rolled back"
    );
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn a_link_to_a_soft_deleted_message_still_locks() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    env.link(chat, id, 1).await;
    env.stream
        .quota
        .db
        .soft_delete("messages", Uuid::from_u128(0xD100_0001))
        .await;

    let outcome = delete(&env, chat, id, None).await.unwrap();

    assert_eq!(outcome, DeleteOutcome::Locked);
}

#[tokio::test]
async fn the_lock_covers_a_row_that_already_has_a_cleanup_status_too() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| row.set("cleanup_status", "pending"))
        .await;
    env.link(chat, id, 1).await;
    let before = env.attachments_dump().await;

    let outcome = delete(&env, chat, id, None).await.unwrap();

    assert_eq!(outcome, DeleteOutcome::Locked);
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn an_attachment_of_another_chat_or_tenant_is_not_found() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let other_chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    let before = env.attachments_dump().await;

    let wrong_chat = delete(&env, other_chat, id, None).await;
    let wrong_id = delete(&env, chat, Uuid::new_v4(), None).await;
    let wrong_tenant = env
        .sea
        .soft_delete(
            &toolkit_security::AccessScope::for_tenant(OTHER_TENANT),
            &request(&env, chat, id, None),
        )
        .await;

    for result in [wrong_chat, wrong_id, wrong_tenant] {
        assert!(
            matches!(
                result,
                Err(DomainError::NotFound {
                    resource: ResourceKind::Attachment
                })
            ),
            "{result:?}"
        );
    }
    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn a_failed_enqueue_rolls_the_delete_back() {
    let env = AttEnv::with(AttOptions {
        enqueue_fails: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    let before = env.attachments_dump().await;

    let result = delete(&env, chat, id, None).await;

    assert!(result.is_err(), "{result:?}");
    assert_eq!(env.attachments_dump().await, before);
    let healthy = env
        .healthy_repo()
        .soft_delete(&env.scope(), &request(&env, chat, id, None))
        .await;
    assert!(
        matches!(healthy, Ok(DeleteOutcome::Deleted)),
        "a registered queue lets it through: {healthy:?}"
    );
}

#[tokio::test]
async fn an_outbox_database_failure_rolls_the_delete_back() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;
    env.fail_outbox().await;
    let before = env.attachments_dump().await;

    let result = delete(&env, chat, id, None).await;

    assert!(result.is_err(), "{result:?}");
    assert_eq!(env.attachments_dump().await, before);
    env.heal("fail_outbox").await;
    assert!(matches!(
        delete(&env, chat, id, None).await,
        Ok(DeleteOutcome::Deleted)
    ));
}

#[tokio::test]
async fn an_oversized_payload_is_an_error_and_leaves_the_row_unchanged() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("provider_file_id", "f".repeat(70_000))
        })
        .await;
    let before = env.attachments_dump().await;

    let result = delete(&env, chat, id, None).await;

    assert!(result.is_err(), "{result:?}");
    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
    let ok = env.seed(chat, 2, |row| row).await;
    assert!(
        matches!(
            delete(&env, chat, ok, None).await,
            Ok(DeleteOutcome::Deleted)
        ),
        "a normal payload is enqueued fine: the size is what failed"
    );
}

#[tokio::test]
async fn concurrent_deletes_enqueue_exactly_one_message() {
    let env = AttEnv::with(AttOptions {
        file_db: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    let results = futures::future::join_all((0..4).map(|_| delete(&env, chat, id, None))).await;

    let deleted = results
        .iter()
        .filter(|r| matches!(r, Ok(DeleteOutcome::Deleted)))
        .count();
    let already = results
        .iter()
        .filter(|r| matches!(r, Ok(DeleteOutcome::AlreadyDeleted)))
        .count();
    assert_eq!((deleted, already), (1, 3), "{results:?}");
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

#[tokio::test]
async fn a_provider_file_stored_just_before_the_delete_is_in_its_payload() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending").null("provider_file_id")
        })
        .await;
    // The delete flow read the row `pending` without a file; the upload stores
    // its file id right before the delete transaction starts.
    env.sea
        .set_uploaded(USER_TENANT, id, "file-stored-late", env.now())
        .await
        .unwrap();

    delete(&env, chat, id, None).await.unwrap();

    let messages = env.cleanup_messages().await;
    assert_eq!(messages[0]["provider_file_id"], "file-stored-late");
}
