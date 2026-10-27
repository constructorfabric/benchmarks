#![allow(clippy::wildcard_imports)]
//! Attachment tests: upload lifecycle, limits, deletion rules, message
//! attachment validation, citations.

use uuid::Uuid;

use super::*;
use crate::api::rest::dto::StreamMessageRequest;
use crate::domain::attachment_service::{UploadInput, UploadPlan};
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;

/// Upload a file through the attachment service.
async fn upload(
    bed: &StreamBed,
    chat_id: Uuid,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<crate::infra::storage::entity::attachments::Model, DomainError> {
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat_id, mime)
        .await?;
    bed.services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            UploadInput {
                filename: filename.to_owned(),
                content_type: mime.to_owned(),
                bytes: bytes::Bytes::from(bytes.to_vec()),
            },
        )
        .await
}

#[tokio::test]
async fn document_upload_ready_via_vector_store() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let att = upload(&bed, chat.id, "doc.txt", "text/plain", b"hello document")
        .await
        .expect("upload");
    assert_eq!(att.status, "ready");
    assert_eq!(att.attachment_kind, "document");
    assert!(att.for_file_search);
    assert!(att.provider_file_id.is_some());

    // The provider got the file and the vector store entry.
    let files = bed.gateway.files.lock().expect("lock");
    assert_eq!(files.len(), 1);
    let stores = bed.gateway.vector_stores.lock().expect("lock");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores.values().next().expect("store").len(), 1);

    // GET detail.
    let detail = bed
        .services
        .attachments
        .get(&bed.bed.security, chat.id, att.id)
        .await
        .expect("get");
    assert_eq!(detail.status, crate::api::rest::dto::AttachmentStatusDto::Ready);
    assert!(detail.doc_summary.is_none());
    assert!(detail.img_thumbnail.is_none());
}

#[tokio::test]
async fn image_upload_ready_with_thumbnail() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // A tiny valid PNG (1x1).
    let png: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let att = upload(&bed, chat.id, "pic.png", "image/png", png)
        .await
        .expect("upload");
    assert_eq!(att.status, "ready");
    assert_eq!(att.attachment_kind, "image");
    assert!(!att.for_file_search);
    assert!(!att.for_code_interpreter);
    // Not indexed in any vector store.
    let stores = bed.gateway.vector_stores.lock().expect("lock");
    assert!(stores.is_empty());
    // Thumbnail generated (1x1 webp).
    let detail = bed
        .services
        .attachments
        .get(&bed.bed.security, chat.id, att.id)
        .await
        .expect("get");
    assert!(detail.img_thumbnail.is_some(), "thumbnail present");
    let thumb = detail.img_thumbnail.expect("thumb");
    assert_eq!(thumb.content_type, "image/webp");
}

#[tokio::test]
async fn unsupported_mime_rejected() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let err = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "application/x-msdownload")
        .await;
    assert!(matches!(err, Err(DomainError::UnsupportedContentType)));
}

#[tokio::test]
async fn per_chat_document_limit() {
    let mut cfg = crate::config::MiniChatConfig::default();
    cfg.rag.max_documents_per_chat = 1;
    let bed = stream_bed_cfg(cfg).await;
    let chat = create_chat(&bed.bed, None, None).await;
    upload(&bed, chat.id, "a.txt", "text/plain", b"a").await.expect("first");
    let err = upload(&bed, chat.id, "b.txt", "text/plain", b"b")
        .await;
    assert!(matches!(err, Err(DomainError::DocumentLimit)));
}

#[tokio::test]
async fn per_chat_storage_limit_counts_images() {
    let mut cfg = crate::config::MiniChatConfig::default();
    cfg.rag.max_total_upload_mb_per_chat = 1; // 1 MiB
    let bed = stream_bed_cfg(cfg).await;
    let chat = create_chat(&bed.bed, None, None).await;
    let big = vec![b'x'; 800 * 1024];
    upload(&bed, chat.id, "a.txt", "text/plain", &big).await.expect("first");
    let err = upload(&bed, chat.id, "b.txt", "text/plain", &big)
        .await;
    assert!(matches!(err, Err(DomainError::StorageLimit)));
}

#[tokio::test]
async fn attachment_deletion_rules() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let att = upload(&bed, chat.id, "doc.txt", "text/plain", b"hello")
        .await
        .expect("upload");

    // Delete an unreferenced attachment: 204.
    bed.services
        .attachments
        .delete(&bed.bed.security, chat.id, att.id)
        .await
        .expect("delete");
    // Idempotent.
    bed.services
        .attachments
        .delete(&bed.bed.security, chat.id, att.id)
        .await
        .expect("delete again");
    // GET of the deleted attachment -> 404.
    let err = bed
        .services
        .attachments
        .get(&bed.bed.security, chat.id, att.id)
        .await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));

    // A referenced attachment cannot be deleted.
    let att2 = upload(&bed, chat.id, "doc2.txt", "text/plain", b"world")
        .await
        .expect("upload");
    // Reference it in a message.
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "about the doc".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![att2.id]),
                web_search: None,
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    let err = bed
        .services
        .attachments
        .delete(&bed.bed.security, chat.id, att2.id)
        .await;
    assert!(matches!(err, Err(DomainError::AttachmentLocked)));
}

#[tokio::test]
async fn message_attachment_validation() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let att = upload(&bed, chat.id, "doc.txt", "text/plain", b"hello")
        .await
        .expect("upload");

    // Unknown id -> invalid_attachment.
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![Uuid::new_v4()]),
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::InvalidAttachment)));

    // Duplicate ids -> invalid_attachment.
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![att.id, att.id]),
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::InvalidAttachment)));

    // Valid reference works.
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![att.id]),
                web_search: None,
            },
        )
        .await
        .expect("send");
    if let StreamSetup::Live { receiver, .. } = setup {
        drain(receiver).await;
    }
    // The message carries the attachment summary.
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let user = page.items.iter().find(|m| m.role == crate::api::rest::dto::MessageRoleDto::User).expect("user");
    assert_eq!(user.attachments.len(), 1);
    assert_eq!(user.attachments[0].attachment_id, att.id);
    assert_eq!(user.attachments[0].filename, "doc.txt");
    // The provider request included the file_search tool.
    let reqs = bed.gateway.requests.lock().expect("lock");
    let tools = reqs[0]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|t| t["type"] == "file_search"), "file_search sent: {tools:?}");
    let vs_ids = tools[0]["vector_store_ids"].as_array().expect("vs ids");
    assert_eq!(vs_ids.len(), 1);
}

#[tokio::test]
async fn citations_map_file_ids_to_attachment_ids() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let att = upload(&bed, chat.id, "doc.txt", "text/plain", b"hello")
        .await
        .expect("upload");
    let provider_file_id = att.provider_file_id.clone().expect("file id");
    let resp_id = format!("resp-{}", Uuid::new_v4().as_simple());
    let response = serde_json::json!({
        "response": {
            "id": resp_id,
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "answer with citation",
                    "annotations": [{
                        "type": "file_citation",
                        "file_id": provider_file_id,
                        "filename": "doc.txt",
                        "index": 0,
                    }],
                }],
            }],
            "usage": {"input_tokens": 3, "output_tokens": 2},
        }
    });
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream {
            events: vec![(
                "response.completed".to_owned(),
                serde_json::to_string(&response).expect("json"),
            )],
        },
    );
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let citations = events.iter().find_map(|e| match e {
        MiniChatSseEvent::Citations(c) => Some(c),
        _ => None,
    });
    let citations = citations.expect("citations event");
    assert_eq!(citations.items.len(), 1);
    let c = &citations.items[0];
    assert_eq!(c.source, crate::api::rest::dto::CitationSource::File);
    assert_eq!(c.attachment_id, Some(att.id));
    assert_eq!(c.title, "doc.txt");
    assert!(c.snippet.is_empty(), "file citations have empty snippet");
}

#[tokio::test]
async fn foreign_attachment_ids_rejected() {
    let bed = stream_bed().await;
    let chat_a = create_chat(&bed.bed, None, None).await;
    let chat_b = create_chat(&bed.bed, None, None).await;
    let att = upload(&bed, chat_a.id, "doc.txt", "text/plain", b"hello")
        .await
        .expect("upload");
    // Referencing an attachment of another chat -> invalid_attachment.
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat_b.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![att.id]),
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::InvalidAttachment)));
}
