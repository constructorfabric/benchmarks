#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: `GET /v1/chats/{id}/attachments/{attachment_id}`
//! (`cpt-cf-mini-chat-flow-attachments-get-attachment`,
//! `cpt-cf-mini-chat-dod-attachments-get-attachment`).

use std::collections::BTreeSet;

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::json;
use uuid::Uuid;

use crate::attachment_test_env::{ATTACHMENT_TYPE, AttEnv, AttOptions};
use crate::chat_test_env::{CHAT_PEP_TYPE, CHAT_TYPE, ts};
use crate::test_doubles::{
    OTHER_TENANT, OTHER_USER, PdpReply, USER_TENANT, other_tenant_ctx, other_user_ctx,
};

async fn setup() -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    (env, chat)
}

fn keys(json: &serde_json::Value) -> BTreeSet<String> {
    json.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

// ================================================================================================
// The detail
// ================================================================================================

#[tokio::test]
async fn a_ready_document_has_exactly_the_required_keys() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("filename", "report.pdf")
                .set("content_type", "application/pdf")
                .set("size_bytes", 1234_i64)
        })
        .await;

    let reply = env.get_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        keys(&reply.json),
        set(&[
            "id",
            "filename",
            "content_type",
            "size_bytes",
            "status",
            "kind",
            "created_at"
        ])
    );
    assert_eq!(reply.json["id"], id.to_string());
    assert_eq!(reply.json["filename"], "report.pdf");
    assert_eq!(reply.json["content_type"], "application/pdf");
    assert_eq!(reply.json["size_bytes"], 1234);
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(reply.json["kind"], "document");
    assert_eq!(reply.json["created_at"], "2024-01-01T00:00:00Z");
    assert!(
        reply
            .header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
}

#[tokio::test]
async fn the_detail_never_carries_an_internal_column() {
    let (env, chat) = setup().await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("secondary_file_id", "file_ant_5")
                .set("secondary_status", "uploaded")
                .set("cleanup_status", "done")
                .set("doc_summary", "a summary")
                .set("summary_model", "gpt-x")
                .set("summary_updated_at", ts(3))
        })
        .await;

    let reply = env.get_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    for key in [
        "provider_file_id",
        "secondary_file_id",
        "secondary_status",
        "vector_store_id",
        "storage_backend",
        "cleanup_status",
        "for_file_search",
        "for_code_interpreter",
        "chat_id",
        "tenant_id",
        "uploaded_by_user_id",
        "updated_at",
        "deleted_at",
        "doc_summary",
        "summary_updated_at",
        "summary_model",
    ] {
        assert!(reply.json.get(key).is_none(), "{key}: {}", reply.text);
    }
    assert!(!reply.text.contains("file-prov1") && !reply.text.contains("file_ant_5"));
    assert!(
        !reply.text.contains("null"),
        "optional keys are omitted, never null: {}",
        reply.text
    );
}

#[tokio::test]
async fn every_status_is_reported_and_only_a_failed_row_has_an_error_code() {
    let (env, chat) = setup().await;
    for (n, status) in [(1, "pending"), (2, "uploaded"), (3, "ready")] {
        let id = env
            .seed(chat, n, |row| {
                row.set("status", status).set("error_code", "upload_failed")
            })
            .await;

        let reply = env.get_attachment(chat, id).await;

        assert_eq!(reply.json["status"], status);
        assert!(
            reply.json.get("error_code").is_none(),
            "{status}: {}",
            reply.text
        );
    }
    for (n, code) in [
        (4, "upload_failed"),
        (5, "vector_store_failed"),
        (6, "indexing_failed"),
        (7, "upload_abandoned"),
    ] {
        let id = env
            .seed(chat, n, |row| {
                row.set("status", "failed").set("error_code", code)
            })
            .await;

        let reply = env.get_attachment(chat, id).await;

        assert_eq!(reply.json["status"], "failed");
        assert_eq!(reply.json["error_code"], code);
    }
}

#[tokio::test]
async fn a_ready_image_with_a_stored_thumbnail_has_img_thumbnail() {
    let (env, chat) = setup().await;
    let bytes = vec![0xFF_u8, 0xFE, 1, 2, 3];
    let id = env
        .seed(chat, 1, |row| {
            row.set("attachment_kind", "image")
                .set("for_file_search", false)
                .set("img_thumbnail", bytes.clone())
                .set("img_thumbnail_width", 128_i32)
                .set("img_thumbnail_height", 64_i32)
        })
        .await;

    let reply = env.get_attachment(chat, id).await;

    assert_eq!(reply.json["kind"], "image");
    assert_eq!(
        reply.json["img_thumbnail"],
        json!({
            "content_type": "image/webp",
            "width": 128,
            "height": 64,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        })
    );
}

#[tokio::test]
async fn the_thumbnail_is_omitted_unless_the_row_is_a_ready_image_with_bytes() {
    let (env, chat) = setup().await;
    let thumb = |row: crate::infra::db::test_support::Row| {
        row.set("img_thumbnail", vec![1_u8, 2, 3])
            .set("img_thumbnail_width", 10_i32)
            .set("img_thumbnail_height", 10_i32)
    };
    let image_without = env
        .seed(chat, 1, |row| {
            row.set("attachment_kind", "image")
                .set("for_file_search", false)
        })
        .await;
    let document_with = env.seed(chat, 2, thumb).await;
    let pending_image = env
        .seed(chat, 3, |row| {
            thumb(row.set("attachment_kind", "image").set("status", "pending"))
        })
        .await;
    let failed_image = env
        .seed(chat, 4, |row| {
            thumb(row.set("attachment_kind", "image").set("status", "failed"))
        })
        .await;

    for id in [image_without, document_with, pending_image, failed_image] {
        let reply = env.get_attachment(chat, id).await;

        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            reply.json.get("img_thumbnail").is_none(),
            "{id}: {}",
            reply.text
        );
    }
}

#[tokio::test]
async fn a_get_writes_nothing() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;
    let before = env.attachments_dump().await;

    let reply = env.get_attachment(chat, id).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "a GET never calls the provider"
    );
}

// ================================================================================================
// 404
// ================================================================================================

#[tokio::test]
async fn an_attachment_that_is_not_visible_is_404_with_the_attachment_type() {
    let (env, chat) = setup().await;
    let other_chat = env.chat("m-oa").await;
    let deleted = env.seed(chat, 1, |row| row.set("deleted_at", ts(5))).await;
    let in_other_chat = env.seed(other_chat, 2, |row| row).await;
    let other_uploader = env
        .seed(chat, 3, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;
    let other_tenant = env
        .seed(chat, 4, |row| row.set("tenant_id", OTHER_TENANT))
        .await;

    for (label, id) in [
        ("unknown", Uuid::new_v4()),
        ("soft-deleted", deleted),
        ("of another chat", in_other_chat),
        ("uploaded by another user", other_uploader),
        ("of another tenant", other_tenant),
    ] {
        let reply = env.get_attachment(chat, id).await;

        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{label}: {}",
            reply.text
        );
        reply.assert_not_found(ATTACHMENT_TYPE);
    }
}

#[tokio::test]
async fn other_uploaders_and_unknown_ids_are_indistinguishable() {
    let (env, chat) = setup().await;
    let other = env
        .seed(chat, 1, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;

    let a = env.get_attachment(chat, other).await;
    let b = env.get_attachment(chat, Uuid::new_v4()).await;

    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.text);
    assert_eq!(a.status, b.status);
    assert_eq!(a.json["context"], b.json["context"]);
    assert_eq!(a.json["detail"], b.json["detail"]);
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_404_with_the_chat_type() {
    let env = AttEnv::new().await;
    let own = env.chat("m-oa").await;
    let id = env.seed(own, 1, |row| row).await;
    let deleted = env.chat("m-oa").await;
    let in_deleted = env.seed(deleted, 2, |row| row).await;
    env.delete_chat(deleted).await;
    let foreign = env.chat_of(USER_TENANT, OTHER_USER, "m-oa").await;
    let in_foreign = env
        .seed(foreign, 3, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;

    for (label, ctx, chat, attachment) in [
        (
            "unknown chat",
            crate::test_doubles::user_ctx(),
            Uuid::new_v4(),
            id,
        ),
        (
            "soft-deleted chat",
            crate::test_doubles::user_ctx(),
            deleted,
            in_deleted,
        ),
        (
            "another user's chat",
            crate::test_doubles::user_ctx(),
            foreign,
            in_foreign,
        ),
        ("mine, asked by another user", other_user_ctx(), own, id),
        ("mine, asked by another tenant", other_tenant_ctx(), own, id),
    ] {
        let reply = env.get_attachment_as(&ctx, chat, attachment).await;

        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{label}: {}",
            reply.text
        );
        reply.assert_not_found(CHAT_TYPE);
    }
}

#[tokio::test]
async fn path_ids_that_are_not_uuids_are_400_invalid_path_params() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    for uri in [
        format!("/mini-chat/v1/chats/not-a-uuid/attachments/{id}"),
        format!("/mini-chat/v1/chats/{chat}/attachments/not-a-uuid"),
    ] {
        let reply = env
            .send(
                "GET",
                &uri,
                &crate::test_doubles::user_ctx(),
                None,
                axum::body::Body::empty(),
            )
            .await;

        reply.assert_invalid("path", "invalid_path_params");
    }
}

// ================================================================================================
// Authorization
// ================================================================================================

#[tokio::test]
async fn the_get_asks_the_pdp_for_read_attachment_on_the_chat() {
    let (env, chat) = setup().await;
    let id = env.seed(chat, 1, |row| row).await;

    env.get_attachment(chat, id).await;

    let requests = env.stream.quota.pdp.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].action.name, "read_attachment");
    assert_eq!(requests[0].resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(requests[0].resource.id, Some(chat));
    assert!(requests[0].context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_is_403_and_a_pdp_failure_is_503_with_retry_after_5() {
    for (pdp, status) in [(PdpReply::Deny, 403_u16), (PdpReply::Failure, 503)] {
        let env = AttEnv::with(AttOptions {
            pdp,
            ..AttOptions::default()
        })
        .await;
        let chat = env.chat("m-oa").await;
        let id = env.seed(chat, 1, |row| row).await;

        let reply = env.get_attachment(chat, id).await;

        assert_eq!(reply.status.as_u16(), status, "{}", reply.text);
        if status == 403 {
            assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");
        } else {
            assert_eq!(reply.header("retry-after"), Some("5"));
        }
    }
}

#[tokio::test]
async fn a_pdp_that_scopes_by_the_chat_id_does_not_hide_the_attachments_of_the_chat() {
    let env = AttEnv::with(AttOptions {
        pdp: PdpReply::AllowChatConstraints,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env.seed(chat, 1, |row| row).await;

    let reply = env.get_attachment(chat, id).await;

    assert_eq!(
        reply.status,
        StatusCode::OK,
        "the chat id is not an attachment id: {}",
        reply.text
    );
}

#[tokio::test]
async fn a_pdp_that_returns_only_a_tenant_predicate_still_isolates_the_owner() {
    let env = AttEnv::new().await;
    let foreign = env.chat_of(USER_TENANT, OTHER_USER, "m-oa").await;
    let id = env
        .seed(foreign, 1, |row| row.set("uploaded_by_user_id", OTHER_USER))
        .await;

    let reply = env.get_attachment(foreign, id).await;

    reply.assert_not_found(CHAT_TYPE);
}
