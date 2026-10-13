#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: images
//! (`cpt-cf-mini-chat-dod-attachments-images`,
//! `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`,
//! `-upload-secondary-copy`, `cpt-cf-mini-chat-state-attachments-secondary-copy`):
//! the WebP thumbnail in the detail and the row, the Anthropic secondary copy and
//! its tolerated failures.

use std::io::Cursor;

use axum::http::StatusCode;
use base64::Engine as _;
use image::ImageFormat;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::attachment_test_env::{ALIAS, ANTHROPIC_ALIAS, AttEnv, AttOptions, parts_of, png_bytes};
use crate::chat_test_env::Reply;
use crate::config::ThumbnailConfig;
use crate::test_doubles::{Fault, ProxyReply, USER_TENANT};

async fn setup(model: &str) -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    env.script_happy();
    let chat = env.chat(model).await;
    (env, chat)
}

fn attachment_id(reply: &Reply) -> Uuid {
    Uuid::parse_str(reply.json["id"].as_str().unwrap()).unwrap()
}

fn decode_thumbnail(reply: &Reply) -> image::DynamicImage {
    let data = reply.json["img_thumbnail"]["data_base64"]
        .as_str()
        .expect("a thumbnail");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    image::load_from_memory_with_format(&bytes, ImageFormat::WebP).expect("a WebP image")
}

// ================================================================================================
// Thumbnails
// ================================================================================================

#[tokio::test]
async fn a_png_is_ready_with_a_webp_thumbnail_fitted_in_128_by_128() {
    let (env, chat) = setup("m-oa").await;

    let reply = env
        .upload(chat, "photo.png", "image/png", &png_bytes(300, 150))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(reply.json["kind"], "image");
    assert_eq!(reply.json["content_type"], "image/png");
    let thumbnail = &reply.json["img_thumbnail"];
    assert_eq!(thumbnail["content_type"], "image/webp");
    assert_eq!(
        (thumbnail["width"].as_i64(), thumbnail["height"].as_i64()),
        (Some(128), Some(64))
    );
    let decoded = decode_thumbnail(&reply);
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(thumbnail["data_base64"].as_str().unwrap())
        .unwrap();
    assert!(bytes.len() <= ThumbnailConfig::default().max_bytes);
    let (_, row) = env.only_att().await;
    assert_eq!(row.thumbnail, Some(bytes));
    assert_eq!(
        (row.thumbnail_width, row.thumbnail_height),
        (Some(128), Some(64))
    );
    assert_eq!(row.error_code, None);
}

#[tokio::test]
async fn the_provider_gets_the_image_with_purpose_assistants_and_no_vector_store_call() {
    let (env, chat) = setup("m-oa").await;
    let png = png_bytes(300, 150);

    let reply = env.upload(chat, "photo.png", "image/png", &png).await;

    assert_eq!(env.sequence(), vec![format!("POST /{ALIAS}/v1/files")]);
    let parts = parts_of(&env.file_uploads()[0]);
    assert_eq!(parts[0].name, "purpose");
    assert_eq!(parts[0].body, b"assistants");
    assert_eq!(
        parts[1].filename.as_deref(),
        Some(format!("{chat}_{}.png", attachment_id(&reply)).as_str())
    );
    assert_eq!(parts[1].content_type.as_deref(), Some("image/png"));
    assert_eq!(parts[1].body, png);
    assert!(env.stores().await.is_empty());
    let (_, row) = env.only_att().await;
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!(
        row.secondary_status, "not_attempted",
        "not an Anthropic chat"
    );
}

#[tokio::test]
async fn jpeg_gif_and_webp_images_get_thumbnails_too() {
    let (env, chat) = setup("m-oa").await;
    let rgb = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        256,
        256,
        image::Rgb([9, 99, 199]),
    ));
    for (name, content_type, format) in [
        ("a.jpg", "image/jpeg", ImageFormat::Jpeg),
        ("a.gif", "image/gif", ImageFormat::Gif),
        ("a.webp", "image/webp", ImageFormat::WebP),
    ] {
        let mut out = Cursor::new(Vec::new());
        rgb.write_to(&mut out, format).unwrap();

        let reply = env
            .upload(chat, name, content_type, &out.into_inner())
            .await;

        assert_eq!(reply.status, StatusCode::CREATED, "{name}: {}", reply.text);
        let decoded = decode_thumbnail(&reply);
        assert_eq!((decoded.width(), decoded.height()), (128, 128), "{name}");
    }
}

#[tokio::test]
async fn a_corrupt_image_is_still_ready_without_a_thumbnail_and_without_an_error_code() {
    let (env, chat) = setup("m-oa").await;

    let reply = env
        .upload(chat, "broken.png", "image/png", b"\x89PNG not really")
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["status"], "ready");
    assert!(reply.json.get("img_thumbnail").is_none(), "{}", reply.text);
    assert!(reply.json.get("error_code").is_none());
    let (_, row) = env.only_att().await;
    assert_eq!(row.thumbnail, None);
    assert_eq!(row.error_code, None);
    assert_eq!(row.status, "ready");
}

#[tokio::test]
async fn an_image_above_the_decode_limit_gets_no_thumbnail_and_no_secondary_copy_but_is_uploaded() {
    let mut options = AttOptions::default();
    options.thumbnail.max_decode_bytes = 300;
    let env = AttEnv::with(options).await;
    env.script_happy();
    let chat = env.chat("m-an").await;
    let png = png_bytes(64, 64);
    assert!(png.len() > 300);

    let reply = env.upload(chat, "big.png", "image/png", &png).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(reply.json.get("img_thumbnail").is_none());
    assert_eq!(env.file_uploads().len(), 1);
    assert_eq!(
        parts_of(&env.file_uploads()[0])[1].body,
        png,
        "the held bytes still go to the provider"
    );
    assert!(
        env.oagw
            .proxied()
            .iter()
            .all(|c| !c.uri.contains(ANTHROPIC_ALIAS)),
        "no secondary copy: {:?}",
        env.sequence()
    );
    assert_eq!(env.only_att().await.1.secondary_status, "not_attempted");
}

#[tokio::test]
async fn a_thumbnail_above_the_byte_limit_is_skipped_without_failing_the_upload() {
    let mut options = AttOptions::default();
    options.thumbnail.max_bytes = 16;
    let env = AttEnv::with(options).await;
    env.script_happy();
    let chat = env.chat("m-oa").await;

    let reply = env
        .upload(chat, "a.png", "image/png", &png_bytes(300, 150))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(reply.json.get("img_thumbnail").is_none());
    assert_eq!(env.only_att().await.1.error_code, None);
}

#[tokio::test]
async fn the_thumbnail_comes_from_the_configured_box() {
    let mut options = AttOptions::default();
    options.thumbnail.width = 64;
    options.thumbnail.height = 32;
    let env = AttEnv::with(options).await;
    env.script_happy();
    let chat = env.chat("m-oa").await;

    let reply = env
        .upload(chat, "a.png", "image/png", &png_bytes(300, 150))
        .await;

    assert_eq!(reply.json["img_thumbnail"]["width"], 64);
    assert_eq!(reply.json["img_thumbnail"]["height"], 32);
}

#[tokio::test]
async fn a_later_get_returns_the_same_detail_with_the_thumbnail() {
    let (env, chat) = setup("m-oa").await;
    let created = env
        .upload(chat, "a.png", "image/png", &png_bytes(300, 150))
        .await;

    let got = env.get_attachment(chat, attachment_id(&created)).await;

    assert_eq!(got.json, created.json);
    assert!(got.json["img_thumbnail"]["data_base64"].is_string());
}

#[tokio::test]
async fn an_upload_does_not_check_the_vision_capability_of_the_model() {
    let (env, chat) = setup("m-nov").await;

    let reply = env
        .upload(chat, "a.png", "image/png", &png_bytes(10, 10))
        .await;

    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "vision is checked per turn: {}",
        reply.text
    );
}

// ================================================================================================
// The secondary Anthropic copy
// ================================================================================================

#[tokio::test]
async fn an_image_in_an_anthropic_chat_is_also_copied_to_anthropic() {
    let (env, chat) = setup("m-an").await;
    let png = png_bytes(300, 150);

    let reply = env.upload(chat, "photo.png", "image/png", &png).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["status"], "ready");
    assert_eq!(
        env.sequence(),
        vec![
            format!("POST /{ALIAS}/v1/files"),
            format!("POST /{ANTHROPIC_ALIAS}/v1/files"),
        ],
        "primary storage in the rag provider, then the secondary copy"
    );
    let secondary = &env.oagw.proxied()[1];
    assert_eq!(secondary.uri, format!("/{ANTHROPIC_ALIAS}/v1/files"));
    assert_eq!(secondary.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        secondary.header("anthropic-beta"),
        Some("files-api-2025-04-14")
    );
    let parts = parts_of(secondary);
    assert_eq!(parts.len(), 1, "only the file part, no purpose");
    assert_eq!(parts[0].name, "file");
    assert_eq!(
        parts[0].filename.as_deref(),
        Some(format!("{chat}_{}.png", attachment_id(&reply)).as_str())
    );
    assert_eq!(parts[0].content_type.as_deref(), Some("image/png"));
    assert_eq!(parts[0].body, png);
    let (_, row) = env.only_att().await;
    assert_eq!(row.secondary_status, "uploaded");
    assert_eq!(row.secondary_file_id.as_deref(), Some("file_ant_1"));
    assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(row.status, "ready");
    assert!(reply.json["img_thumbnail"].is_object());
    assert!(
        !reply.text.contains("file_ant_1"),
        "no provider id in the response"
    );
}

#[tokio::test]
async fn a_failing_secondary_copy_is_recorded_and_the_image_is_still_ready() {
    for fail in [
        ProxyReply::json(500, &json!({})),
        ProxyReply::json(200, &json!({})),
        ProxyReply::Error(Fault::DeadlineExceeded),
    ] {
        let (env, chat) = setup("m-an").await;
        let once = std::sync::Mutex::new(Some(fail));
        env.oagw
            .on_path("POST", &format!("/{ANTHROPIC_ALIAS}/v1/files"), move |_| {
                once.lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| ProxyReply::json(200, &json!({ "id": "x" })))
            });

        let reply = env
            .upload(chat, "photo.png", "image/png", &png_bytes(64, 64))
            .await;

        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        assert_eq!(reply.json["status"], "ready");
        let (_, row) = env.only_att().await;
        assert_eq!(row.secondary_status, "failed");
        assert_eq!(row.secondary_file_id, None);
        assert_eq!(row.status, "ready");
        assert_eq!(env.stream.quota.outbox_message_count().await, 0);
        assert!(env.deletes().is_empty());
    }
}

#[tokio::test]
async fn a_database_error_on_the_secondary_columns_never_fails_the_upload() {
    let (env, chat) = setup("m-an").await;
    env.fail_updates(
        "no_secondary",
        "NEW.secondary_status <> OLD.secondary_status",
    )
    .await;

    let reply = env
        .upload(chat, "photo.png", "image/png", &png_bytes(64, 64))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(env.only_att().await.1.status, "ready");
}

#[tokio::test]
async fn only_images_of_anthropic_chats_are_copied() {
    let (env, openai_chat) = setup("m-oa").await;
    let azure_chat = env.chat("m-az").await;
    env.oagw.on_path_json(
        "POST",
        "/az.test/openai/files",
        200,
        &json!({ "id": "file-az" }),
    );

    let on_openai = env
        .upload(openai_chat, "a.png", "image/png", &png_bytes(8, 8))
        .await;
    let on_azure = env
        .upload(azure_chat, "a.png", "image/png", &png_bytes(8, 8))
        .await;

    assert_eq!(on_openai.status, StatusCode::CREATED, "{}", on_openai.text);
    assert_eq!(on_azure.status, StatusCode::CREATED, "{}", on_azure.text);
    assert!(
        env.oagw
            .proxied()
            .iter()
            .all(|c| !c.uri.contains(ANTHROPIC_ALIAS)),
        "{:?}",
        env.sequence()
    );
}

#[tokio::test]
async fn a_database_error_on_ready_after_a_secondary_copy_cleans_up_both_files() {
    use toolkit_db::outbox::{LeasedMessageHandler as _, MessageResult};
    let (env, chat) = setup("m-an").await;
    env.fail_updates("no_ready", "NEW.status = 'ready'").await;

    let reply = env
        .upload(chat, "photo.png", "image/png", &png_bytes(64, 64))
        .await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    let (id, row) = env.only_att().await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = env.cleanup_messages().await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["event_type"], "attachment_upload_failed");
    assert_eq!(messages[0]["provider_file_id"], "file-1");
    assert_eq!(messages[0]["attachment_kind"], "image");
    assert_eq!(messages[0]["attachment_id"], id.to_string());
    assert_eq!(messages[0]["tenant_id"], USER_TENANT.to_string());
    assert_eq!(
        messages[0]["secondary_ref"],
        json!({ "file_id": "file_ant_1", "provider_kind": "anthropic", "upstream_alias": ANTHROPIC_ALIAS })
    );
    assert_eq!(messages[0].as_object().unwrap().len(), 10);
    assert!(env.deletes().is_empty(), "the request deletes nothing");

    let result = env
        .cleanup_handler()
        .handle(&AttEnv::message(&messages[0], 0))
        .await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(
        env.deletes()
            .iter()
            .map(|d| d.uri.clone())
            .collect::<Vec<_>>(),
        vec![
            format!("/{ALIAS}/v1/files/file-1"),
            format!("/{ANTHROPIC_ALIAS}/v1/files/file_ant_1"),
        ]
    );
    assert_eq!(env.att(id).await.cleanup_status.as_deref(), Some("done"));
    let _: Option<Value> = None;
}
