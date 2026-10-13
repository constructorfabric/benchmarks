#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the single-statement CAS transitions and the write-first failure
//! transaction of the attachment status machine
//! (`cpt-cf-mini-chat-state-attachments-attachment-status`,
//! `cpt-cf-mini-chat-algo-attachments-upload-provider-file`, `-mark-ready`,
//! `-fail-upload`).

use uuid::Uuid;

use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::domain::error::DomainError;
use crate::domain::service::attachments::model::{
    FailOutcome, FailWithCleanup, FailureCode, ReadyOutcome, SecondaryUpdate, Thumbnail,
};
use crate::domain::service::attachments::payload::{CleanupEvent, SecondaryRef};
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::test_doubles::{OTHER_TENANT, USER_TENANT};

async fn seeded(env: &AttEnv, chat: Uuid, n: u128, status: &str, file: Option<&str>) -> Uuid {
    env.seed(chat, n, |row| {
        let row = row.set("status", status);
        match file {
            Some(file) => row.set("provider_file_id", file),
            None => row.null("provider_file_id"),
        }
    })
    .await
}

fn thumb() -> Thumbnail {
    Thumbnail {
        bytes: vec![9, 8, 7, 6],
        width: 128,
        height: 64,
    }
}

// ================================================================================================
// pending -> uploaded
// ================================================================================================

#[tokio::test]
async fn a_pending_row_moves_to_uploaded_with_its_provider_file_id() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "pending", None).await;

    let moved = env
        .sea
        .set_uploaded(USER_TENANT, id, "file-new", env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert!(moved);
    assert_eq!(snap.status, "uploaded");
    assert_eq!(snap.provider_file_id.as_deref(), Some("file-new"));
    assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
}

#[tokio::test]
async fn the_uploaded_cas_requires_a_live_pending_row_of_the_tenant() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let cases = [
        seeded(&env, chat, 1, "uploaded", Some("file-1")).await,
        seeded(&env, chat, 2, "ready", Some("file-2")).await,
        seeded(&env, chat, 3, "failed", None).await,
        env.seed(chat, 4, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await,
        env.seed(chat, 5, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("cleanup_status", "pending")
        })
        .await,
    ];
    let pending = seeded(&env, chat, 6, "pending", None).await;
    let before = env.attachments_dump().await;

    for id in cases {
        assert!(
            !env.sea
                .set_uploaded(USER_TENANT, id, "file-x", env.now())
                .await
                .unwrap(),
            "{id}"
        );
    }
    assert!(
        !env.sea
            .set_uploaded(OTHER_TENANT, pending, "file-x", env.now())
            .await
            .unwrap(),
        "another tenant"
    );
    assert!(
        !env.sea
            .set_uploaded(USER_TENANT, Uuid::new_v4(), "file-x", env.now())
            .await
            .unwrap()
    );

    assert_eq!(
        env.attachments_dump().await,
        before,
        "no guarded row was written"
    );
}

// ================================================================================================
// uploaded -> ready
// ================================================================================================

#[tokio::test]
async fn an_uploaded_row_with_a_provider_file_becomes_ready() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "uploaded", Some("file-1")).await;

    let outcome = env
        .sea
        .set_ready(USER_TENANT, id, None, env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert_eq!(outcome, ReadyOutcome::Ready);
    assert_eq!(snap.status, "ready");
    assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
    assert_eq!(snap.thumbnail, None);
    assert_eq!(snap.thumbnail_width, None);
}

#[tokio::test]
async fn the_ready_transition_stores_the_thumbnail_of_an_image() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("attachment_kind", "image")
                .set("for_file_search", false)
        })
        .await;

    let outcome = env
        .sea
        .set_ready(USER_TENANT, id, Some(&thumb()), env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert_eq!(outcome, ReadyOutcome::Ready);
    assert_eq!(snap.thumbnail, Some(vec![9, 8, 7, 6]));
    assert_eq!(
        (snap.thumbnail_width, snap.thumbnail_height),
        (Some(128), Some(64))
    );
}

#[tokio::test]
async fn the_ready_cas_requires_an_uploaded_live_row_with_a_provider_file() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let cases = [
        seeded(&env, chat, 1, "uploaded", None).await,
        seeded(&env, chat, 2, "pending", Some("file-2")).await,
        seeded(&env, chat, 3, "failed", Some("file-3")).await,
        seeded(&env, chat, 4, "ready", Some("file-4")).await,
        env.seed(chat, 5, |row| {
            row.set("status", "uploaded")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await,
        env.seed(chat, 6, |row| {
            row.set("status", "uploaded")
                .set("cleanup_status", "pending")
        })
        .await,
    ];

    for id in cases {
        let outcome = env
            .sea
            .set_ready(USER_TENANT, id, None, env.now())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReadyOutcome::Taken {
                chat_deleted: false
            },
            "{id}"
        );
    }
    assert_eq!(
        env.att(cases[3]).await.updated_at,
        crate::chat_test_env::ts(0),
        "untouched"
    );
    let other = seeded(&env, chat, 7, "uploaded", Some("file-7")).await;
    assert!(matches!(
        env.sea
            .set_ready(OTHER_TENANT, other, None, env.now())
            .await
            .unwrap(),
        ReadyOutcome::Taken { .. }
    ));
    assert_eq!(env.att(other).await.status, "uploaded");
}

#[tokio::test]
async fn a_taken_ready_cas_reports_whether_the_chat_is_soft_deleted() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("cleanup_status", "pending")
        })
        .await;
    env.delete_chat(chat).await;

    let outcome = env
        .sea
        .set_ready(USER_TENANT, id, None, env.now())
        .await
        .unwrap();

    assert_eq!(outcome, ReadyOutcome::Taken { chat_deleted: true });
    assert_eq!(
        env.att(id).await.status,
        "uploaded",
        "a row of a deleted chat never becomes ready"
    );
}

// ================================================================================================
// failure without a provider file
// ================================================================================================

#[tokio::test]
async fn a_row_without_a_provider_file_is_failed_with_its_code_and_no_cleanup() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "pending", None).await;

    let moved = env
        .sea
        .set_failed(USER_TENANT, id, FailureCode::UploadFailed, env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert!(moved);
    assert_eq!(snap.status, "failed");
    assert_eq!(snap.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(snap.cleanup_status, None);
    assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn an_uploaded_row_can_be_failed_too_and_every_code_is_written() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    for (n, code, text) in [
        (1, FailureCode::UploadFailed, "upload_failed"),
        (2, FailureCode::VectorStoreFailed, "vector_store_failed"),
        (3, FailureCode::IndexingFailed, "indexing_failed"),
        (4, FailureCode::UploadAbandoned, "upload_abandoned"),
    ] {
        let id = seeded(&env, chat, n, "uploaded", Some("file-x")).await;

        assert!(
            env.sea
                .set_failed(USER_TENANT, id, code, env.now())
                .await
                .unwrap()
        );

        assert_eq!(env.att(id).await.error_code.as_deref(), Some(text));
    }
}

#[tokio::test]
async fn the_failure_update_requires_a_live_pending_or_uploaded_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let cases = [
        seeded(&env, chat, 1, "ready", Some("file-1")).await,
        seeded(&env, chat, 2, "failed", None).await,
        env.seed(chat, 3, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await,
    ];

    for id in cases {
        assert!(
            !env.sea
                .set_failed(USER_TENANT, id, FailureCode::UploadFailed, env.now())
                .await
                .unwrap(),
            "{id}"
        );
    }
    assert_eq!(env.att(cases[0]).await.status, "ready");
    assert_eq!(env.att(cases[2]).await.status, "pending");
}

// ================================================================================================
// failure with a provider file: one write-first transaction
// ================================================================================================

fn request(
    env: &AttEnv,
    chat: Uuid,
    id: Uuid,
    code: FailureCode,
    with_orphan: bool,
) -> FailWithCleanup {
    let event = match code {
        FailureCode::IndexingFailed => CleanupEvent::AttachmentIndexingFailed,
        _ => CleanupEvent::AttachmentUploadFailed,
    };
    FailWithCleanup {
        tenant_id: USER_TENANT,
        chat_id: chat,
        attachment_id: id,
        code,
        provider_file_id: "file-held".to_owned(),
        now: env.now(),
        message: env.cleanup_message(event, chat, id, Some("file-held")),
        orphan: with_orphan.then(|| {
            env.cleanup_message(
                CleanupEvent::AttachmentOrphanFile,
                chat,
                id,
                Some("file-held"),
            )
        }),
    }
}

#[tokio::test]
async fn the_failure_transaction_writes_the_row_and_enqueues_its_message_together() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "uploaded", Some("file-held")).await;
    let request = request(&env, chat, id, FailureCode::IndexingFailed, false);

    let outcome = env.sea.fail_with_cleanup(&request).await.unwrap();

    let snap = env.att(id).await;
    assert_eq!(outcome, FailOutcome::Failed);
    assert_eq!(snap.status, "failed");
    assert_eq!(snap.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(snap.provider_file_id.as_deref(), Some("file-held"));
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0], AttEnv::payload_json(&request.message));
    assert_eq!(
        env.cleanup_partitions().await,
        vec![i64::from(env.partition_of(USER_TENANT, chat))]
    );
}

#[tokio::test]
async fn a_pending_row_gets_the_provider_file_id_held_in_memory() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "pending", None).await;
    let request = request(&env, chat, id, FailureCode::UploadFailed, true);

    let outcome = env.sea.fail_with_cleanup(&request).await.unwrap();

    let snap = env.att(id).await;
    assert_eq!(outcome, FailOutcome::Failed);
    assert_eq!(snap.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(snap.provider_file_id.as_deref(), Some("file-held"));
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(
        messages.len(),
        1,
        "the orphan variant is not enqueued when the CAS wins"
    );
    assert_eq!(messages[0]["event_type"], "attachment_upload_failed");
}

#[tokio::test]
async fn a_lost_cas_with_an_in_memory_file_enqueues_the_orphan_message_alone() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await;
    let before = env.attachments_dump().await;
    let request = request(&env, chat, id, FailureCode::UploadFailed, true);

    let outcome = env.sea.fail_with_cleanup(&request).await.unwrap();

    assert_eq!(outcome, FailOutcome::OrphanEnqueued);
    assert_eq!(env.attachments_dump().await, before, "the row is untouched");
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["event_type"], "attachment_orphan_file");
    assert_eq!(messages[0]["provider_file_id"], "file-held");
}

#[tokio::test]
async fn a_lost_cas_without_an_orphan_message_rolls_back_and_enqueues_nothing() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let taken = [
        env.seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("deleted_at", crate::chat_test_env::ts(5))
        })
        .await,
        env.seed(chat, 2, |row| {
            row.set("status", "uploaded")
                .set("cleanup_status", "pending")
        })
        .await,
        seeded(&env, chat, 3, "ready", Some("file-3")).await,
        seeded(&env, chat, 4, "failed", Some("file-4")).await,
    ];
    let before = env.attachments_dump().await;

    for id in taken {
        let request = request(&env, chat, id, FailureCode::UploadFailed, false);

        let outcome = env.sea.fail_with_cleanup(&request).await.unwrap();

        assert_eq!(outcome, FailOutcome::RowTaken, "{id}");
    }

    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn a_failed_enqueue_rolls_the_failure_back() {
    let env = AttEnv::with(AttOptions {
        enqueue_fails: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "uploaded", Some("file-held")).await;
    let before = env.attachments_dump().await;

    let result = env
        .sea
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::IndexingFailed, false))
        .await;

    assert!(result.is_err(), "{result:?}");
    assert_eq!(
        env.attachments_dump().await,
        before,
        "the row write is rolled back with it"
    );
    let healthy = env
        .healthy_repo()
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::IndexingFailed, false))
        .await
        .unwrap();
    assert_eq!(
        healthy,
        FailOutcome::Failed,
        "with a registered queue the same failure goes through"
    );
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

#[tokio::test]
async fn an_outbox_database_failure_rolls_the_failure_back() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "uploaded", Some("file-held")).await;
    env.fail_outbox().await;
    let before = env.attachments_dump().await;

    let result = env
        .sea
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::UploadFailed, true))
        .await;

    assert!(
        matches!(
            result,
            Err(DomainError::Database(_) | DomainError::Internal(_))
        ),
        "{result:?}"
    );
    assert_eq!(env.attachments_dump().await, before);
    env.heal("fail_outbox").await;
    let healed = env
        .sea
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::UploadFailed, true))
        .await
        .unwrap();
    assert_eq!(
        healed,
        FailOutcome::Failed,
        "the same transaction succeeds once the outbox works"
    );
}

#[tokio::test]
async fn a_failing_row_write_enqueues_nothing() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "uploaded", Some("file-held")).await;
    env.fail_updates("no_failed", "NEW.status = 'failed'").await;

    let result = env
        .sea
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::UploadFailed, true))
        .await;

    assert!(result.is_err());
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
    assert_eq!(env.att(id).await.status, "uploaded");
    env.heal("no_failed").await;
    let healed = env
        .sea
        .fail_with_cleanup(&request(&env, chat, id, FailureCode::UploadFailed, true))
        .await
        .unwrap();
    assert_eq!(healed, FailOutcome::Failed);
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

// ================================================================================================
// the row-less orphan enqueue
// ================================================================================================

#[tokio::test]
async fn the_orphan_message_is_enqueued_alone_without_touching_any_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = seeded(&env, chat, 1, "failed", None).await;
    let before = env.attachments_dump().await;
    let message = env.cleanup_message(
        CleanupEvent::AttachmentOrphanFile,
        chat,
        id,
        Some("file-lost"),
    );

    env.sea.enqueue_orphan_file(&message).await.unwrap();

    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(
        env.cleanup_messages().await,
        vec![AttEnv::payload_json(&message)]
    );
    assert_eq!(
        env.cleanup_partitions().await,
        vec![i64::from(env.partition_of(USER_TENANT, chat))]
    );
}

#[tokio::test]
async fn the_orphan_message_does_not_need_a_row() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let message = env.cleanup_message(
        CleanupEvent::AttachmentOrphanFile,
        chat,
        Uuid::new_v4(),
        Some("file-lost"),
    );

    env.sea.enqueue_orphan_file(&message).await.unwrap();

    assert_eq!(env.cleanup_messages().await.len(), 1);
    assert_eq!(env.att_count().await, 0);
}

#[tokio::test]
async fn a_failing_orphan_enqueue_is_an_error() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.fail_outbox().await;
    let message = env.cleanup_message(
        CleanupEvent::AttachmentOrphanFile,
        chat,
        Uuid::new_v4(),
        Some("f"),
    );

    assert!(env.sea.enqueue_orphan_file(&message).await.is_err());
    env.heal("fail_outbox").await;
    env.sea.enqueue_orphan_file(&message).await.unwrap();
    assert_eq!(env.cleanup_messages().await.len(), 1);
}

// ================================================================================================
// the secondary copy columns
// ================================================================================================

#[tokio::test]
async fn the_secondary_columns_follow_their_state_machine() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("attachment_kind", "image")
        })
        .await;

    env.sea
        .set_secondary(USER_TENANT, id, &SecondaryUpdate::Pending, env.now())
        .await
        .unwrap();
    let pending = env.att(id).await;
    env.sea
        .set_secondary(
            USER_TENANT,
            id,
            &SecondaryUpdate::Uploaded("file_ant_1".to_owned()),
            env.now(),
        )
        .await
        .unwrap();
    let uploaded = env.att(id).await;

    assert_eq!(pending.secondary_status, "pending");
    assert_eq!(
        pending.secondary_provider_kind.as_deref(),
        Some("anthropic")
    );
    assert_eq!(pending.secondary_file_id, None);
    assert_eq!(uploaded.secondary_status, "uploaded");
    assert_eq!(uploaded.secondary_file_id.as_deref(), Some("file_ant_1"));
    assert_eq!(
        uploaded.secondary_provider_kind.as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        uploaded.status, "uploaded",
        "the attachment status is not touched"
    );
}

#[tokio::test]
async fn a_failed_secondary_copy_is_recorded() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row.set("status", "uploaded")).await;
    env.sea
        .set_secondary(USER_TENANT, id, &SecondaryUpdate::Pending, env.now())
        .await
        .unwrap();

    env.sea
        .set_secondary(USER_TENANT, id, &SecondaryUpdate::Failed, env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert_eq!(snap.secondary_status, "failed");
    assert_eq!(snap.secondary_file_id, None);
}

#[tokio::test]
async fn the_secondary_update_is_bound_to_the_tenant() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row.set("status", "uploaded")).await;

    env.sea
        .set_secondary(OTHER_TENANT, id, &SecondaryUpdate::Pending, env.now())
        .await
        .unwrap();

    assert_eq!(env.att(id).await.secondary_status, "not_attempted");
    let _ = SecondaryRef {
        file_id: String::new(),
        upstream_alias: String::new(),
    };
}
