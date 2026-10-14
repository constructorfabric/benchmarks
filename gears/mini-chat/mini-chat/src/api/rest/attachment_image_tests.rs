//! The image upload flow over the router: synchronous provider upload without any vector store,
//! the WebP thumbnail and its safeguards (a skip never fails the upload and never sets
//! `error_code`), the thumbnail stored only in the database, no vision check at upload, and the
//! Anthropic secondary copy (no `purpose`, the kept bytes, a failure that leaves the attachment
//! `ready`, no copy for documents or for images that were not kept).
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image`; algorithms
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail` and `-secondary-copy`; state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy`; definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-images`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::Engine as _;
use image::ImageFormat;

use crate::config::ThumbnailConfig;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, ANTHROPIC_CHAT_MODEL, AttachRigOptions, OPENAI_ALIAS, PDF, assert_created,
    assert_no_provider_text, assert_storage_unavailable, body_id, gif, jpeg, key_set, pdf_bytes,
    png, png_header_only, rig, rig_with, webp,
};
use crate::test_support::storage_mock::{Answer, Route};

fn expected_keys(with_thumbnail: bool) -> std::collections::BTreeSet<String> {
    let mut keys: std::collections::BTreeSet<String> = [
        "id",
        "filename",
        "content_type",
        "size_bytes",
        "status",
        "kind",
        "created_at",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if with_thumbnail {
        keys.insert("img_thumbnail".to_owned());
    }
    keys
}

// ── the upload and the thumbnail ─────────────────────────────────────────────

#[tokio::test]
async fn a_png_upload_is_201_ready_with_a_webp_thumbnail() {
    // inst-img-return-ready, inst-th-return
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "cat.png", "image/png", &png(400, 200))
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["kind"], "image");
    assert_eq!(key_set(&body), expected_keys(true), "{body}");
    let thumbnail = &body["img_thumbnail"];
    assert_eq!(thumbnail["content_type"], "image/webp");
    assert_eq!(thumbnail["width"], 128);
    assert_eq!(thumbnail["height"], 64);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(thumbnail["data_base64"].as_str().unwrap())
        .unwrap();
    let decoded = image::load_from_memory_with_format(&bytes, ImageFormat::WebP).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
}

#[tokio::test]
async fn the_thumbnail_is_stored_in_the_row_and_never_uploaded() {
    // inst-th-storage, inst-img-set-ready
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "cat.png", "image/png", &png(400, 200))
        .await;
    let row = rig.row(chat.id, body_id(&reply)).await;
    assert_eq!(row.status, "ready");
    assert!(
        row.img_thumbnail
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty())
    );
    assert_eq!(
        (row.img_thumbnail_width, row.img_thumbnail_height),
        (Some(128), Some(64))
    );
    assert_eq!(row.error_code, None);
    // only the original image reaches the provider
    assert_eq!(rig.storage.count_of(Route::FilesUpload), 1);
    let call = rig.storage.calls_of(Route::FilesUpload).remove(0);
    assert_eq!(&call.part("file").unwrap().data[..], &png(400, 200)[..]);
}

#[tokio::test]
async fn an_image_row_has_both_purpose_flags_false_and_never_reaches_a_vector_store() {
    // inst-img-no-index
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(
        &rig.upload(chat.id, "cat.png", "image/png", &png(40, 40))
            .await,
        "ready",
    );
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.attachment_kind, "image");
    assert!(!row.for_file_search && !row.for_code_interpreter);
    let routes: Vec<Route> = rig.storage.calls().iter().map(|call| call.route).collect();
    assert_eq!(routes, vec![Route::FilesUpload]);
    assert!(rig.vector_stores().await.is_empty());
}

#[tokio::test]
async fn jpeg_gif_and_webp_images_get_thumbnails_too() {
    for (name, mime, bytes) in [
        ("a.jpg", "image/jpeg", jpeg(300, 150)),
        ("a.gif", "image/gif", gif(300, 150)),
        ("a.webp", "image/webp", webp(300, 150)),
    ] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let body = assert_created(&rig.upload(chat.id, name, mime, &bytes).await, "ready");
        assert_eq!(
            body["img_thumbnail"]["content_type"], "image/webp",
            "{name}"
        );
        assert_eq!(body["content_type"], mime);
    }
}

#[tokio::test]
async fn an_octet_stream_image_takes_its_type_from_the_extension() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(
            chat.id,
            "photo.png",
            "application/octet-stream",
            &png(32, 32),
        )
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["content_type"], "image/png");
    assert_eq!(body["kind"], "image");
}

#[tokio::test]
async fn an_image_on_a_model_without_vision_input_uploads_successfully() {
    // the vision check is a send-time guard; the upload never checks it
    let rig = rig().await;
    let chat = rig.std_chat().await; // the standard model has no VISION_INPUT
    assert_created(
        &rig.upload(chat.id, "a.png", "image/png", &png(16, 16))
            .await,
        "ready",
    );
}

// ── skips never fail the upload ──────────────────────────────────────────────

#[tokio::test]
async fn a_pixel_bomb_header_gives_ready_without_a_thumbnail_and_without_an_error_code() {
    // inst-th-prescreen
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(
            chat.id,
            "bomb.png",
            "image/png",
            &png_header_only(40_000, 40_000),
        )
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(key_set(&body), expected_keys(false), "{body}");
    let row = rig.only_row(chat.id).await;
    assert!(row.img_thumbnail.is_none());
    assert_eq!(row.error_code, None);
    assert_eq!(row.status, "ready");
}

#[tokio::test]
async fn an_image_above_the_decode_cap_is_uploaded_without_a_thumbnail() {
    // inst-th-not-kept: larger than thumbnail.max_decode_bytes
    let rig = rig_with(AttachRigOptions::new().with_thumbnail(ThumbnailConfig {
        max_decode_bytes: 1000,
        ..ThumbnailConfig::default()
    }))
    .await;
    let chat = rig.std_chat().await;
    let big = png(120, 120);
    assert!(big.len() > 1000, "fixture too small: {}", big.len());
    let reply = rig.upload(chat.id, "big.png", "image/png", &big).await;
    let body = assert_created(&reply, "ready");
    assert_eq!(key_set(&body), expected_keys(false));
    assert!(rig.only_row(chat.id).await.img_thumbnail.is_none());
}

#[tokio::test]
async fn an_encoded_thumbnail_above_max_bytes_is_skipped() {
    // inst-th-encode: no retry at lower quality
    let rig = rig_with(AttachRigOptions::new().with_thumbnail(ThumbnailConfig {
        max_bytes: 10,
        ..ThumbnailConfig::default()
    }))
    .await;
    let chat = rig.std_chat().await;
    let body = assert_created(
        &rig.upload(chat.id, "a.png", "image/png", &png(300, 300))
            .await,
        "ready",
    );
    assert_eq!(key_set(&body), expected_keys(false));
    assert_eq!(rig.only_row(chat.id).await.error_code, None);
}

#[tokio::test]
async fn undecodable_image_bytes_are_ready_without_a_thumbnail() {
    // inst-th-decode: a decode failure is not an upload failure
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "broken.png", "image/png", b"definitely not a png")
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(key_set(&body), expected_keys(false));
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "ready");
    assert_eq!(row.error_code, None);
}

// ── image upload failures ────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_failure_for_an_image_is_503_and_the_row_failed_upload_failed() {
    // inst-img-return-failure
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::FilesUpload, Answer::status(500));
    let reply = rig
        .upload(chat.id, "a.png", "image/png", &png(16, 16))
        .await;
    assert_storage_unavailable(&reply);
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert!(row.img_thumbnail.is_none());
    assert_eq!(
        rig.uploads(),
        vec![("image".to_owned(), "error".to_owned())]
    );
    assert_eq!(rig.permits.available(), 10);
}

#[tokio::test]
async fn the_image_201_carries_the_image_kind_in_the_upload_metrics() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let png_bytes = png(16, 16);
    assert_created(
        &rig.upload(chat.id, "a.png", "image/png", &png_bytes).await,
        "ready",
    );
    assert_eq!(rig.uploads(), vec![("image".to_owned(), "ok".to_owned())]);
    assert_eq!(
        rig.metrics.upload_bytes(),
        vec![("image".to_owned(), png_bytes.len() as u64)]
    );
}

#[tokio::test]
async fn the_image_response_never_carries_a_provider_identifier() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "a.png", "image/png", &png(16, 16))
        .await;
    assert_no_provider_text(&reply.text);
}

// ── the Anthropic secondary copy ─────────────────────────────────────────────

#[tokio::test]
async fn an_anthropic_chat_image_gets_a_secondary_copy_without_a_purpose() {
    // inst-sec-upload, inst-sec-set-uploaded: the primary goes to the RAG provider
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    let image = png(64, 64);
    let reply = rig.upload(chat.id, "cat.png", "image/png", &image).await;
    let body = assert_created(&reply, "ready");
    let uploads = rig.storage.calls_of(Route::FilesUpload);
    assert_eq!(uploads.len(), 2);
    let primary = uploads
        .iter()
        .find(|call| call.alias == OPENAI_ALIAS)
        .expect("primary upload");
    assert_eq!(primary.text_part("purpose").as_deref(), Some("assistants"));
    let secondary = uploads
        .iter()
        .find(|call| call.alias == ANTHROPIC_ALIAS)
        .expect("secondary upload");
    assert!(
        secondary.part("purpose").is_none(),
        "the secondary upload carries a purpose"
    );
    assert_eq!(secondary.parts.len(), 1);
    assert_eq!(
        &secondary.part("file").unwrap().data[..],
        &image[..],
        "the kept bytes"
    );
    let row = rig.row(chat.id, body_id(&reply)).await;
    assert_eq!(row.secondary_status, "uploaded");
    assert!(row.secondary_file_id.is_some());
    assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));
    assert_eq!(row.status, "ready");
    // the client never sees the secondary id
    assert!(!reply.text.contains(&row.secondary_file_id.unwrap()));
    assert_eq!(body["kind"], "image");
}

#[tokio::test]
async fn a_failed_secondary_upload_leaves_the_attachment_ready_with_secondary_failed() {
    // inst-sec-set-failed: the Anthropic adapter cannot reference this image
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    rig.storage
        .set_default_for(ANTHROPIC_ALIAS, Route::FilesUpload, Answer::status(500));
    let reply = rig
        .upload(chat.id, "cat.png", "image/png", &png(64, 64))
        .await;
    let body = assert_created(&reply, "ready");
    assert!(body.get("error_code").is_none(), "{body}");
    let row = rig.row(chat.id, body_id(&reply)).await;
    assert_eq!(row.status, "ready");
    assert_eq!(row.error_code, None);
    assert_eq!(row.secondary_status, "failed");
    assert_eq!(row.secondary_file_id, None);
    assert!(
        row.provider_file_id.is_some(),
        "the primary upload is untouched"
    );
    assert_eq!(rig.uploads(), vec![("image".to_owned(), "ok".to_owned())]);
}

#[tokio::test]
async fn a_chat_not_served_by_anthropic_gets_no_secondary_copy() {
    // inst-sec-skip
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "cat.png", "image/png", &png(64, 64))
        .await;
    assert_created(&reply, "ready");
    assert_eq!(rig.storage.calls_of(Route::FilesUpload).len(), 1);
    assert_eq!(
        rig.only_row(chat.id).await.secondary_status,
        "not_attempted"
    );
}

#[tokio::test]
async fn a_document_in_an_anthropic_chat_gets_no_secondary_copy() {
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    assert_created(
        &rig.upload(chat.id, "a.pdf", PDF, &pdf_bytes()).await,
        "ready",
    );
    assert!(
        rig.storage
            .calls_of(Route::FilesUpload)
            .iter()
            .all(|call| call.alias != ANTHROPIC_ALIAS)
    );
    assert_eq!(
        rig.only_row(chat.id).await.secondary_status,
        "not_attempted"
    );
}

#[tokio::test]
async fn an_anthropic_image_that_was_not_kept_in_memory_gets_no_secondary_copy() {
    // inst-sec-skip: the image bytes exceeded thumbnail.max_decode_bytes
    let rig = rig_with(AttachRigOptions::new().with_thumbnail(ThumbnailConfig {
        max_decode_bytes: 1000,
        ..ThumbnailConfig::default()
    }))
    .await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    let big = png(120, 120);
    assert!(big.len() > 1000);
    assert_created(
        &rig.upload(chat.id, "big.png", "image/png", &big).await,
        "ready",
    );
    assert!(
        rig.storage
            .calls_of(Route::FilesUpload)
            .iter()
            .all(|call| call.alias != ANTHROPIC_ALIAS)
    );
    assert_eq!(
        rig.only_row(chat.id).await.secondary_status,
        "not_attempted"
    );
}

#[tokio::test]
async fn the_secondary_copy_is_not_attempted_when_the_primary_upload_failed() {
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    rig.storage
        .set_default_for(OPENAI_ALIAS, Route::FilesUpload, Answer::status(500));
    let reply = rig
        .upload(chat.id, "cat.png", "image/png", &png(64, 64))
        .await;
    assert_storage_unavailable(&reply);
    assert!(
        rig.storage
            .calls_of(Route::FilesUpload)
            .iter()
            .all(|call| call.alias != ANTHROPIC_ALIAS)
    );
    assert_eq!(
        rig.only_row(chat.id).await.secondary_status,
        "not_attempted"
    );
}

#[tokio::test]
async fn the_secondary_upload_runs_under_the_s2s_context() {
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    rig.upload(chat.id, "cat.png", "image/png", &png(64, 64))
        .await;
    let s2s = crate::test_support::send::provider_s2s();
    let secondary = rig
        .storage
        .calls_of(Route::FilesUpload)
        .into_iter()
        .find(|call| call.alias == ANTHROPIC_ALIAS)
        .unwrap();
    assert_eq!(
        (secondary.subject, secondary.tenant),
        (s2s.subject_id(), s2s.subject_tenant_id())
    );
}
