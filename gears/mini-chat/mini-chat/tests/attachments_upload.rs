#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Attachment upload, get and delete (DESIGN "Upload Attachment", "Get Attachment", "Attachment
//! Deletion", the upload error table, B.7, B.8): the REST contract, the provider calls an upload
//! makes through the fake OAGW, the rows it writes, the validation and limit errors, storage
//! failures and tenant / owner isolation.
//!
//! Indexing timing (in-progress documents, the 25 s deadline, background indexing) is in
//! `attachments_indexing.rs`, tool availability in `attachments_tools.rs`, cleanup handlers in
//! `attachments_cleanup.rs`, the reaper in `upload_reaper.rs`.

mod attachments;
mod common;

use attachments::{
    DOCUMENT_DETAIL_KEYS, DOCX, JPEG, PDF, PNG, PPTX, Part, RowSeed, TEXT,
    assert_attachment_not_found, assert_keys, assert_no_provider_ids, assert_thumbnail,
    assert_timestamp, assert_unavailable, call_lines, calls, calls_to, chat_rows, contains_bytes,
    delete_attachment, delete_ok, detail_id, expect_created, file_path, files_path, get_attachment,
    jpeg, multipart_body, multipart_content_type, multipart_open, pdf_bytes, png, provider_error,
    request_header, row, row_count, script_document, script_file, script_plain_file,
    script_store_create, store_rows, upload, upload_parts, upload_pdf, upload_ready_document,
    upload_ready_image, upload_request_raw, vector_store_files_path, vector_stores_path,
    wait_calls,
};
use common::{
    CHAT_RESOURCE_TYPE, DISABLED_MODEL, PREMIUM_MODEL, STANDARD_MODEL, XLSX_CONTENT_TYPE,
    assert_failed_precondition, assert_invalid_argument, assert_not_found, assert_out_of_range,
    assert_problem, assert_quota_exhausted, catalog_entry, chat_on, delete_chat, env_user_chat,
    env_with_kill_switches, env_with_policy, expect_status, expect_stream, merge, new_chat, policy,
    send, set_chat_model,
};
use http::StatusCode;
use mini_chat::test_support::{AuthzMode, ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

async fn env_with_config(patch: Value) -> TestEnv {
    TestEnvBuilder::new().gear_config(patch).build().await
}

// ===========================================================================
// Document upload: response, provider calls, rows
// ===========================================================================

#[tokio::test]
async fn a_document_upload_returns_201_with_a_ready_detail() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let data = pdf_bytes(2048);

    let resp = upload(&env, &user, &chat, "report.pdf", PDF, &data).await;

    let detail = expect_created(&resp);
    assert_keys(&detail, &DOCUMENT_DETAIL_KEYS);
    assert_eq!(detail["filename"], json!("report.pdf"));
    assert_eq!(detail["content_type"], json!(PDF));
    assert_eq!(detail["size_bytes"], json!(2048));
    assert_eq!(detail["status"], json!("ready"));
    assert_eq!(detail["kind"], json!("document"));
    assert_timestamp(&detail["created_at"]);
    assert!(
        detail.get("doc_summary").is_none()
            && detail.get("summary_updated_at").is_none()
            && detail.get("img_thumbnail").is_none()
            && detail.get("error_code").is_none(),
        "null members are omitted: {detail}"
    );
    assert_no_provider_ids(&resp.json().to_string());
}

#[tokio::test]
async fn a_document_upload_makes_the_provider_calls_of_the_upload_sequence() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let data = pdf_bytes(2048);

    let detail = expect_created(&upload(&env, &user, &chat, "report.pdf", PDF, &data).await);

    assert_eq!(
        call_lines(&env),
        vec![
            "POST /llm.test/v1/files".to_owned(),
            "POST /llm.test/v1/vector_stores".to_owned(),
            "POST /llm.test/v1/vector_stores/vs_chat1/files".to_owned(),
        ],
        "file upload, vector store create, add file; no polling when the add reports completed"
    );
    let file_call = &calls_to(&env, "POST", &files_path())[0];
    let content_type = request_header(file_call, "content-type").unwrap_or_default();
    assert!(
        content_type.starts_with("multipart/form-data"),
        "the file goes up as multipart: {content_type}"
    );
    assert!(contains_bytes(&file_call.body_bytes, b"name=\"purpose\""));
    assert!(contains_bytes(&file_call.body_bytes, b"assistants"));
    assert!(contains_bytes(&file_call.body_bytes, b"name=\"file\""));
    assert!(
        contains_bytes(&file_call.body_bytes, &data),
        "the file bytes are the file part"
    );
    let add = &calls_to(&env, "POST", &vector_store_files_path("vs_chat1"))[0];
    let body = add.body_json.clone().expect("a JSON body");
    assert_eq!(body["file_id"], json!("file-doc1"));
    let attribute = body["attributes"]["attachment_id"].as_str().unwrap();
    assert_eq!(
        attribute.replace('-', ""),
        detail["id"].as_str().unwrap().replace('-', ""),
        "the vector store file carries {{attachment_id}}: {body}"
    );
    assert_eq!(
        body["attributes"].as_object().unwrap().len(),
        1,
        "no other attribute is set: {body}"
    );
}

#[tokio::test]
async fn a_document_upload_persists_the_attachment_and_the_chat_vector_store() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let detail =
        expect_created(&upload(&env, &user, &chat, "report.pdf", PDF, &pdf_bytes(2048)).await);

    let stored = row(&env, detail_id(&detail)).await;

    assert_eq!(stored.status, "ready");
    assert_eq!(stored.provider_file_id.as_deref(), Some("file-doc1"));
    assert_eq!(stored.kind, "document");
    assert!(stored.for_file_search && !stored.for_code_interpreter);
    assert_eq!(stored.storage_backend, "openai");
    assert_eq!(stored.uploaded_by_hex, common::uuid_hex(user.user_id));
    assert_eq!(stored.tenant_hex, common::uuid_hex(user.tenant_id));
    assert_eq!(
        stored.chat_hex,
        common::uuid_hex(Uuid::parse_str(&chat).unwrap())
    );
    assert_eq!(stored.filename, "report.pdf");
    assert_eq!(stored.content_type.as_deref(), Some(PDF));
    assert_eq!(stored.size_bytes, Some(2048));
    assert!(stored.error_code.is_none() && stored.deleted_at.is_none());
    assert!(stored.cleanup_status.is_none());
    assert_eq!(stored.thumbnail_len, None, "documents have no thumbnail");
    let vector_stores = store_rows(&env, &chat).await;
    assert_eq!(vector_stores.len(), 1);
    assert_eq!(
        vector_stores[0].vector_store_id.as_deref(),
        Some("vs_chat1")
    );
    assert_eq!(vector_stores[0].provider, "openai");
    assert_eq!(
        vector_stores[0].file_count, 0,
        "file_count is reserved and stays 0"
    );
}

#[tokio::test]
async fn the_provider_file_carries_the_structured_name_of_the_chat_and_attachment() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let detail =
        expect_created(&upload(&env, &user, &chat, "report.pdf", PDF, &pdf_bytes(512)).await);
    let id = detail["id"].as_str().unwrap().to_owned();

    let file_call = &calls_to(&env, "POST", &files_path())[0];

    let hyphenated = format!("{chat}_{id}.pdf");
    let simple = format!("{}_{}.pdf", chat.replace('-', ""), id.replace('-', ""));
    assert!(
        contains_bytes(&file_call.body_bytes, hyphenated.as_bytes())
            || contains_bytes(&file_call.body_bytes, simple.as_bytes()),
        "the provider filename is {{chat_id}}_{{attachment_id}}.{{ext}}"
    );
}

#[tokio::test]
async fn get_returns_the_same_detail_as_the_upload() {
    let (env, user, chat) = env_user_chat().await;
    let document =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    script_plain_file(&env, "file-img1");
    let image = expect_created(&upload(&env, &user, &chat, "cat.png", PNG, &png(256, 128)).await);

    for created in [document, image] {
        let resp = get_attachment(&env, &user, &chat, created["id"].as_str().unwrap()).await;

        expect_status(&resp, StatusCode::OK);
        assert_eq!(resp.json(), created);
    }
}

#[tokio::test]
async fn a_second_document_reuses_the_chat_vector_store() {
    let (env, user, chat) = env_user_chat().await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;

    upload_ready_document(&env, &user, &chat, "b.pdf", "file-doc2", "vs_chat1", false).await;

    assert_eq!(
        calls_to(&env, "POST", &vector_stores_path()).len(),
        1,
        "one vector store per chat"
    );
    let adds = calls_to(&env, "POST", &vector_store_files_path("vs_chat1"));
    assert_eq!(adds.len(), 2);
    assert_eq!(
        adds[1].body_json.as_ref().unwrap()["file_id"],
        json!("file-doc2")
    );
    assert_eq!(store_rows(&env, &chat).await.len(), 1);
}

#[tokio::test]
async fn documents_of_different_chats_get_separate_vector_stores() {
    let (env, user, chat_a) = env_user_chat().await;
    let chat_b = new_chat(&env, &user).await;
    upload_ready_document(&env, &user, &chat_a, "a.pdf", "file-a", "vs_a", true).await;

    upload_ready_document(&env, &user, &chat_b, "b.pdf", "file-b", "vs_b", true).await;

    assert_eq!(
        store_rows(&env, &chat_a).await[0]
            .vector_store_id
            .as_deref(),
        Some("vs_a")
    );
    assert_eq!(
        store_rows(&env, &chat_b).await[0]
            .vector_store_id
            .as_deref(),
        Some("vs_b")
    );
    assert_eq!(
        calls_to(&env, "POST", &vector_store_files_path("vs_b")).len(),
        1
    );
    assert_eq!(
        calls_to(&env, "POST", &vector_store_files_path("vs_a")).len(),
        1
    );
}

// ===========================================================================
// Kinds, purposes and MIME handling
// ===========================================================================

#[tokio::test]
async fn every_document_mime_type_is_a_document_routed_to_file_search() {
    let (env, user, chat) = env_user_chat().await;
    let mut first = true;
    for (index, content_type) in [
        PDF,
        DOCX,
        PPTX,
        TEXT,
        "text/markdown",
        "text/html",
        "application/json",
    ]
    .into_iter()
    .enumerate()
    {
        script_document(&env, &format!("file-doc{index}"), "vs_chat1", first);
        first = false;

        let detail = expect_created(
            &upload(&env, &user, &chat, "f.bin", content_type, &pdf_bytes(64)).await,
        );

        assert_eq!(detail["kind"], json!("document"), "{content_type}");
        assert_eq!(detail["content_type"], json!(content_type));
        let stored = row(&env, detail_id(&detail)).await;
        assert_eq!(stored.kind, "document");
        assert!(
            stored.for_file_search && !stored.for_code_interpreter,
            "{content_type}"
        );
    }
}

#[tokio::test]
async fn every_image_mime_type_is_an_image_that_is_not_indexed() {
    let (env, user, chat) = env_user_chat().await;
    for (index, content_type) in ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .enumerate()
    {
        script_plain_file(&env, &format!("file-img{index}"));

        let detail =
            expect_created(&upload(&env, &user, &chat, "x", content_type, b"not-an-image").await);

        assert_eq!(detail["kind"], json!("image"), "{content_type}");
        assert_eq!(detail["status"], json!("ready"));
        let stored = row(&env, detail_id(&detail)).await;
        assert_eq!(stored.kind, "image");
        assert!(
            !stored.for_file_search && !stored.for_code_interpreter,
            "{content_type}"
        );
    }
    assert!(
        calls(&env)
            .iter()
            .all(|call| !call.path.contains("vector_stores")),
        "images are never indexed"
    );
    assert!(
        store_rows(&env, &chat).await.is_empty(),
        "no vector store for images"
    );
}

#[tokio::test]
async fn an_xlsx_upload_is_routed_to_the_code_interpreter_only() {
    let (env, user, chat) = env_user_chat().await;
    script_plain_file(&env, "file-xlsx1");

    let detail = expect_created(
        &upload(
            &env,
            &user,
            &chat,
            "data.xlsx",
            XLSX_CONTENT_TYPE,
            &pdf_bytes(512),
        )
        .await,
    );

    assert_keys(&detail, &DOCUMENT_DETAIL_KEYS);
    assert_eq!(detail["kind"], json!("document"));
    assert_eq!(detail["status"], json!("ready"));
    let stored = row(&env, detail_id(&detail)).await;
    assert!(!stored.for_file_search && stored.for_code_interpreter);
    assert_eq!(stored.provider_file_id.as_deref(), Some("file-xlsx1"));
    assert_eq!(call_lines(&env), vec!["POST /llm.test/v1/files".to_owned()]);
    assert!(
        store_rows(&env, &chat).await.is_empty(),
        "no vector store for an XLSX"
    );
}

#[tokio::test]
async fn csv_is_accepted_as_text_plain() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-csv", "vs_chat1", true);

    let detail =
        expect_created(&upload(&env, &user, &chat, "table.csv", "text/csv", b"a,b\n1,2\n").await);

    assert_eq!(detail["content_type"], json!("text/plain"));
    assert_eq!(
        row(&env, detail_id(&detail)).await.content_type.as_deref(),
        Some("text/plain")
    );
}

#[tokio::test]
async fn csv_is_unsupported_when_csv_upload_is_off() {
    let env = env_with_config(json!({ "rag": { "allow_csv_upload": false } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = upload(&env, &user, &chat, "table.csv", "text/csv", b"a,b\n").await;

    assert_invalid_argument(&resp, "content_type", "UNSUPPORTED_CONTENT_TYPE");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn octet_stream_takes_the_mime_type_of_the_filename_extension() {
    let (env, user, chat) = env_user_chat().await;
    let cases = [
        ("a.pdf", PDF, "document"),
        ("a.docx", DOCX, "document"),
        ("a.txt", TEXT, "document"),
        ("a.md", "text/markdown", "document"),
        ("a.png", PNG, "image"),
        ("a.xlsx", XLSX_CONTENT_TYPE, "document"),
    ];
    let mut first_document = true;
    for (index, (filename, expected, kind)) in cases.into_iter().enumerate() {
        let file = format!("file-oct{index}");
        if kind == "image" || expected == XLSX_CONTENT_TYPE {
            script_plain_file(&env, &file);
        } else {
            script_document(&env, &file, "vs_chat1", first_document);
            first_document = false;
        }

        let resp = upload(
            &env,
            &user,
            &chat,
            filename,
            "application/octet-stream",
            &png(64, 64),
        )
        .await;

        let detail = expect_created(&resp);
        assert_eq!(detail["content_type"], json!(expected), "{filename}");
        assert_eq!(detail["kind"], json!(kind), "{filename}");
    }
}

#[tokio::test]
async fn octet_stream_with_an_unknown_extension_is_unsupported() {
    let (env, user, chat) = env_user_chat().await;

    for filename in ["archive.bin", "noextension"] {
        let resp = upload(
            &env,
            &user,
            &chat,
            filename,
            "application/octet-stream",
            b"data",
        )
        .await;

        assert_invalid_argument(&resp, "content_type", "UNSUPPORTED_CONTENT_TYPE");
    }
    assert!(
        calls(&env).is_empty(),
        "no provider call for an unsupported type"
    );
}

#[tokio::test]
async fn unsupported_mime_types_are_400_unsupported_content_type() {
    let (env, user, chat) = env_user_chat().await;

    for content_type in [
        "application/zip",
        "video/mp4",
        "image/svg+xml",
        "application/x-msdownload",
    ] {
        let resp = upload(&env, &user, &chat, "f.dat", content_type, b"data").await;

        let problem = assert_invalid_argument(&resp, "content_type", "UNSUPPORTED_CONTENT_TYPE");
        assert_eq!(problem["status"], json!(400));
    }
    assert!(calls(&env).is_empty());
}

// ===========================================================================
// Filename
// ===========================================================================

#[tokio::test]
async fn a_missing_filename_defaults_to_upload() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let data = pdf_bytes(64);
    let part = Part {
        name: "file",
        filename: None,
        content_type: Some(TEXT),
        data: &data,
    };

    let detail = expect_created(&upload_parts(&env, &user, &chat, &[part]).await);

    assert_eq!(detail["filename"], json!("upload"));
    assert_eq!(row(&env, detail_id(&detail)).await.filename, "upload");
}

#[tokio::test]
async fn a_long_filename_is_truncated_to_255_characters_keeping_the_extension() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let long = format!("{}.pdf", "a".repeat(300));

    let detail = expect_created(&upload(&env, &user, &chat, &long, PDF, &pdf_bytes(64)).await);

    let name = detail["filename"].as_str().unwrap();
    assert_eq!(name.chars().count(), 255);
    assert_eq!(name, format!("{}.pdf", "a".repeat(251)));
    assert_eq!(row(&env, detail_id(&detail)).await.filename, name);
}

#[tokio::test]
async fn a_long_filename_without_an_extension_is_cut_at_255_characters() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let long = "b".repeat(300);

    let detail = expect_created(&upload(&env, &user, &chat, &long, TEXT, b"text").await);

    assert_eq!(detail["filename"], json!("b".repeat(255)));
}

#[tokio::test]
async fn a_filename_of_255_characters_is_kept() {
    let (env, user, chat) = env_user_chat().await;
    script_document(&env, "file-doc1", "vs_chat1", true);
    let name = format!("{}.pdf", "c".repeat(251));

    let detail = expect_created(&upload(&env, &user, &chat, &name, PDF, b"text").await);

    assert_eq!(detail["filename"], json!(name));
}

// ===========================================================================
// Images and thumbnails
// ===========================================================================

#[tokio::test]
async fn an_image_upload_returns_a_ready_detail_with_a_thumbnail() {
    let (env, user, chat) = env_user_chat().await;
    script_plain_file(&env, "file-img1");

    let resp = upload(&env, &user, &chat, "wide.png", PNG, &png(640, 320)).await;

    let detail = expect_created(&resp);
    let mut keys = DOCUMENT_DETAIL_KEYS.to_vec();
    keys.push("img_thumbnail");
    assert_keys(&detail, &keys);
    assert_eq!(detail["kind"], json!("image"));
    assert_eq!(detail["status"], json!("ready"));
    assert_eq!(detail["content_type"], json!(PNG));
    let thumbnail = &detail["img_thumbnail"];
    assert_thumbnail(thumbnail);
    assert_eq!(
        (thumbnail["width"].as_u64(), thumbnail["height"].as_u64()),
        (Some(128), Some(64)),
        "fit inside 128x128, aspect ratio preserved"
    );
    assert_no_provider_ids(&resp.json().to_string());
}

#[tokio::test]
async fn a_tall_jpeg_thumbnail_keeps_its_aspect_ratio() {
    let (env, user, chat) = env_user_chat().await;
    script_plain_file(&env, "file-img1");

    let detail =
        expect_created(&upload(&env, &user, &chat, "tall.jpg", JPEG, &jpeg(150, 300)).await);

    assert_thumbnail(&detail["img_thumbnail"]);
    assert_eq!(detail["img_thumbnail"]["width"], json!(64));
    assert_eq!(detail["img_thumbnail"]["height"], json!(128));
}

#[tokio::test]
async fn an_image_upload_stores_the_thumbnail_and_the_routing_flags() {
    let (env, user, chat) = env_user_chat().await;
    script_plain_file(&env, "file-img1");
    let detail = expect_created(&upload(&env, &user, &chat, "wide.png", PNG, &png(640, 320)).await);

    let stored = row(&env, detail_id(&detail)).await;

    assert_eq!(stored.status, "ready");
    assert_eq!(stored.kind, "image");
    assert!(!stored.for_file_search && !stored.for_code_interpreter);
    assert_eq!(stored.provider_file_id.as_deref(), Some("file-img1"));
    assert_eq!(stored.thumbnail_width, Some(128));
    assert_eq!(stored.thumbnail_height, Some(64));
    assert!(stored.thumbnail_len.unwrap() > 0);
    assert!(stored.error_code.is_none());
    assert_eq!(
        call_lines(&env),
        vec!["POST /llm.test/v1/files".to_owned()],
        "the thumbnail is never uploaded to the provider and the image is not indexed"
    );
    assert!(store_rows(&env, &chat).await.is_empty());
}

#[tokio::test]
async fn the_thumbnail_size_follows_the_configuration() {
    let env = env_with_config(json!({ "thumbnail": { "width": 32, "height": 32 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_plain_file(&env, "file-img1");

    let detail = expect_created(&upload(&env, &user, &chat, "wide.png", PNG, &png(100, 50)).await);

    assert_thumbnail(&detail["img_thumbnail"]);
    assert_eq!(detail["img_thumbnail"]["width"], json!(32));
    assert_eq!(detail["img_thumbnail"]["height"], json!(16));
}

#[tokio::test]
async fn a_skipped_or_failed_thumbnail_still_makes_the_image_ready_without_one() {
    let big = png(64, 64);
    assert!(
        big.len() > 200,
        "the fixture image is larger than the decode cap below"
    );
    let cases: Vec<(&str, Value, Vec<u8>)> = vec![
        (
            "undecodable bytes",
            json!({}),
            b"definitely not a png".to_vec(),
        ),
        (
            "encoded thumbnail above thumbnail.max_bytes",
            json!({ "thumbnail": { "max_bytes": 8 } }),
            png(64, 64),
        ),
        (
            "pixels above thumbnail.max_pixels",
            json!({ "thumbnail": { "max_pixels": 100 } }),
            png(64, 64),
        ),
        (
            "image above thumbnail.max_decode_bytes",
            json!({ "thumbnail": { "max_decode_bytes": 200 } }),
            big,
        ),
    ];
    for (name, config, bytes) in cases {
        let env = env_with_config(config).await;
        let user = TestUser::new();
        let chat = new_chat(&env, &user).await;
        script_plain_file(&env, "file-img1");

        let detail = expect_created(&upload(&env, &user, &chat, "a.png", PNG, &bytes).await);

        assert_eq!(detail["status"], json!("ready"), "{name}");
        assert!(detail.get("img_thumbnail").is_none(), "{name}: {detail}");
        assert!(detail.get("error_code").is_none(), "{name}: {detail}");
        let stored = row(&env, detail_id(&detail)).await;
        assert_eq!(stored.thumbnail_len, None, "{name}");
        assert_eq!(
            stored.error_code, None,
            "{name}: a thumbnail failure sets no error code"
        );
        assert_eq!(
            stored.provider_file_id.as_deref(),
            Some("file-img1"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn images_are_rejected_when_the_images_kill_switch_is_on() {
    let env = env_with_kill_switches(json!({ "disable_images": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = upload(&env, &user, &chat, "cat.png", PNG, &png(32, 32)).await;

    assert_failed_precondition(&resp, "images", "FEATURE_DISABLED");
    assert!(calls(&env).is_empty(), "no provider call");
    assert!(
        chat_rows(&env, &chat)
            .await
            .iter()
            .all(|row| row.status != "ready"),
        "no ready attachment"
    );
}

#[tokio::test]
async fn documents_are_still_accepted_when_the_images_kill_switch_is_on() {
    let env = env_with_kill_switches(json!({ "disable_images": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_document(&env, "file-doc1", "vs_chat1", true);

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    assert_eq!(detail["status"], json!("ready"));
}

// ===========================================================================
// Code interpreter availability
// ===========================================================================

#[tokio::test]
async fn an_xlsx_is_rejected_when_the_code_interpreter_kill_switch_is_on() {
    let env = env_with_kill_switches(json!({ "disable_code_interpreter": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = upload(
        &env,
        &user,
        &chat,
        "data.xlsx",
        XLSX_CONTENT_TYPE,
        &pdf_bytes(64),
    )
    .await;

    let problem = assert_invalid_argument(&resp, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(
        problem["context"]["resource_type"],
        json!(attachments::ATTACHMENT_RESOURCE_TYPE)
    );
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn an_xlsx_is_rejected_when_the_chat_model_has_no_code_interpreter() {
    // The harness `gpt-standard` model has `tool_support.code_interpreter = false`.
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let resp = upload(
        &env,
        &user,
        &chat,
        "data.xlsx",
        XLSX_CONTENT_TYPE,
        &pdf_bytes(64),
    )
    .await;

    assert_invalid_argument(&resp, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn documents_are_accepted_when_the_code_interpreter_is_unavailable() {
    let env = env_with_kill_switches(json!({ "disable_code_interpreter": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_document(&env, "file-doc1", "vs_chat1", true);

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    assert_eq!(detail["status"], json!("ready"));
}

// ===========================================================================
// Size limits
// ===========================================================================

#[tokio::test]
async fn a_document_over_the_document_size_limit_is_400_file_too_large() {
    let env = env_with_config(json!({ "rag": { "uploaded_file_max_size_kb": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = upload(&env, &user, &chat, "big.pdf", PDF, &pdf_bytes(1025)).await;

    let problem = assert_out_of_range(&resp, "FILE_TOO_LARGE");
    assert_eq!(
        common::field_violation_reason(&problem, "content_length").as_deref(),
        Some("FILE_TOO_LARGE")
    );
    assert!(calls(&env).is_empty(), "nothing reaches the provider");
}

#[tokio::test]
async fn a_document_exactly_at_the_size_limit_is_accepted() {
    let env = env_with_config(json!({ "rag": { "uploaded_file_max_size_kb": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_document(&env, "file-doc1", "vs_chat1", true);

    let detail = expect_created(&upload(&env, &user, &chat, "ok.pdf", PDF, &pdf_bytes(1024)).await);

    assert_eq!(detail["size_bytes"], json!(1024));
}

#[tokio::test]
async fn images_have_their_own_size_limit() {
    let env = env_with_config(json!({ "rag": { "uploaded_image_max_size_kb": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = png(64, 64);
    assert!(
        image.len() > 1024,
        "the fixture image exceeds the image limit"
    );
    script_document(&env, "file-doc1", "vs_chat1", true);

    let document = upload(&env, &user, &chat, "a.pdf", PDF, &pdf_bytes(image.len())).await;
    let too_large = upload(&env, &user, &chat, "a.png", PNG, &image).await;

    expect_created(&document);
    assert_out_of_range(&too_large, "FILE_TOO_LARGE");
}

#[tokio::test]
async fn the_model_file_size_limit_caps_documents_and_images() {
    let limited = merge(
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        json!({ "general_config": { "max_file_size_mb": 1 } }),
    );
    let env = env_with_policy(policy(vec![
        limited,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let over_one_mib = 1024 * 1024 + 1;

    let document = upload(&env, &user, &chat, "a.pdf", PDF, &pdf_bytes(over_one_mib)).await;
    let image = upload(&env, &user, &chat, "a.png", PNG, &pdf_bytes(over_one_mib)).await;

    assert_out_of_range(&document, "FILE_TOO_LARGE");
    assert_out_of_range(&image, "FILE_TOO_LARGE");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn the_smaller_of_the_configured_and_the_model_limit_applies() {
    let roomy = merge(
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        json!({ "general_config": { "max_file_size_mb": 50 } }),
    );
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "rag": { "uploaded_file_max_size_kb": 2 } }))
        .policy_config(policy(vec![
            roomy,
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ]))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = upload(&env, &user, &chat, "a.pdf", PDF, &pdf_bytes(2049)).await;

    assert_out_of_range(&resp, "FILE_TOO_LARGE");
}

// ===========================================================================
// Per-chat limits
// ===========================================================================

#[tokio::test]
async fn the_per_chat_document_limit_is_429_document_limit() {
    let env = env_with_config(json!({ "rag": { "max_documents_per_chat": 2 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_1", true).await;
    upload_ready_document(&env, &user, &chat, "b.pdf", "file-b", "vs_1", false).await;
    let before = call_lines(&env).len();

    let resp = upload_pdf(&env, &user, &chat).await;

    let problem = assert_quota_exhausted(&resp, "document_limit");
    assert_eq!(problem["status"], json!(429));
    assert_eq!(
        call_lines(&env).len(),
        before,
        "no provider call for the rejected upload"
    );
    let other_chat = new_chat(&env, &user).await;
    upload_ready_document(&env, &user, &other_chat, "c.pdf", "file-c", "vs_2", true).await;
}

#[tokio::test]
async fn images_do_not_count_as_documents() {
    let env = env_with_config(json!({ "rag": { "max_documents_per_chat": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_1", true).await;

    let image = upload_ready_image(&env, &user, &chat, "a.png", "file-img").await;

    assert_eq!(image["kind"], json!("image"));
}

#[tokio::test]
async fn failed_and_deleted_documents_do_not_count_toward_the_document_limit() {
    let env = env_with_config(json!({ "rag": { "max_documents_per_chat": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    env.provider().enqueue("/v1/files", provider_error(500));
    assert_unavailable(&upload_pdf(&env, &user, &chat).await, 10);
    let first = upload_ready_document(&env, &user, &chat, "a.pdf", "file-a", "vs_1", true).await;
    assert_quota_exhausted(&upload_pdf(&env, &user, &chat).await, "document_limit");

    delete_ok(&env, &user, &chat, detail_id(&first)).await;

    upload_ready_document(&env, &user, &chat, "b.pdf", "file-b", "vs_1", false).await;
}

#[tokio::test]
async fn the_per_chat_storage_limit_is_429_storage_limit() {
    let env = env_with_config(json!({ "rag": { "max_total_upload_mb_per_chat": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_document(&env, "file-a", "vs_1", true);
    let first = upload(&env, &user, &chat, "a.pdf", PDF, &pdf_bytes(600 * 1024)).await;
    expect_created(&first);
    let before = call_lines(&env).len();

    let resp = upload(&env, &user, &chat, "b.pdf", PDF, &pdf_bytes(600 * 1024)).await;

    assert_quota_exhausted(&resp, "storage_limit");
    assert_eq!(
        call_lines(&env).len(),
        before,
        "no provider call for the rejected upload"
    );
}

#[tokio::test]
async fn images_count_toward_the_storage_limit() {
    let env = env_with_config(json!({ "rag": { "max_total_upload_mb_per_chat": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    script_plain_file(&env, "file-img");
    expect_created(&upload(&env, &user, &chat, "a.png", PNG, &pdf_bytes(600 * 1024)).await);

    let resp = upload(&env, &user, &chat, "b.pdf", PDF, &pdf_bytes(600 * 1024)).await;

    assert_quota_exhausted(&resp, "storage_limit");
}

#[tokio::test]
async fn failed_uploads_do_not_count_toward_the_storage_limit() {
    let env = env_with_config(json!({ "rag": { "max_total_upload_mb_per_chat": 1 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    env.provider().enqueue("/v1/files", provider_error(500));
    assert_unavailable(
        &upload(&env, &user, &chat, "a.pdf", PDF, &pdf_bytes(600 * 1024)).await,
        10,
    );
    script_document(&env, "file-b", "vs_1", true);
    expect_created(&upload(&env, &user, &chat, "b.pdf", PDF, &pdf_bytes(600 * 1024)).await);

    let resp = upload(&env, &user, &chat, "c.pdf", PDF, &pdf_bytes(600 * 1024)).await;

    assert_quota_exhausted(&resp, "storage_limit");
}

// ===========================================================================
// Request validation (before and while reading the body)
// ===========================================================================

#[tokio::test]
async fn an_unknown_chat_is_404_chat_before_the_body_is_read() {
    let (env, user, _chat) = env_user_chat().await;
    let unknown = Uuid::new_v4().to_string();

    // A body that is not multipart at all: the chat lookup comes first.
    let resp = env
        .request_raw(
            &user,
            upload_request_raw(&unknown, Some("application/json"), b"{}".to_vec()),
        )
        .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn another_users_other_tenants_and_deleted_chats_are_404_chat() {
    let (env, owner, chat) = env_user_chat().await;
    let colleague = TestUser::in_tenant(owner.tenant_id);
    let stranger = TestUser::new();
    let deleted = new_chat(&env, &owner).await;
    delete_chat(&env, &owner, &deleted).await;

    for (user, target) in [(&colleague, &chat), (&stranger, &chat), (&owner, &deleted)] {
        let resp = env
            .request_raw(user, upload_request_raw(target, None, Vec::new()))
            .await;

        assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    }
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_chat_whose_model_left_the_catalog_is_400_invalid_model_before_the_body_is_read() {
    let (env, user, chat) = env_user_chat().await;
    set_chat_model(&env, &chat, "model-that-was-removed").await;

    let resp = env
        .request_raw(&user, upload_request_raw(&chat, None, Vec::new()))
        .await;

    assert_invalid_argument(&resp, "model", "INVALID_MODEL");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_chat_on_a_disabled_catalog_model_can_still_upload() {
    let (env, user, chat) = env_user_chat().await;
    set_chat_model(&env, &chat, DISABLED_MODEL).await;
    script_document(&env, "file-doc1", "vs_chat1", true);

    let resp = upload_pdf(&env, &user, &chat).await;

    assert_eq!(
        expect_created(&resp)["status"],
        json!("ready"),
        "the model is resolved without the enabled filter"
    );
}

#[tokio::test]
async fn a_content_type_without_a_boundary_is_400_boundary_required() {
    let (env, user, chat) = env_user_chat().await;

    for content_type in [Some("multipart/form-data"), Some("application/json"), None] {
        let resp = env
            .request_raw(
                &user,
                upload_request_raw(
                    &chat,
                    content_type,
                    multipart_body(&[Part::file("a.pdf", PDF, b"x")]),
                ),
            )
            .await;

        assert_invalid_argument(&resp, "content_type", "BOUNDARY_REQUIRED");
    }
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_truncated_multipart_body_is_400_multipart_error() {
    let (env, user, chat) = env_user_chat().await;
    let body = multipart_open(&[Part::file("a.pdf", PDF, b"partial content")]);

    let resp = env
        .request_raw(
            &user,
            upload_request_raw(&chat, Some(&multipart_content_type()), body),
        )
        .await;

    assert_invalid_argument(&resp, "multipart", "MULTIPART_ERROR");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_body_without_a_file_field_is_400_missing_file() {
    let (env, user, chat) = env_user_chat().await;

    let resp = upload_parts(&env, &user, &chat, &[Part::field("note", "hello")]).await;

    assert_invalid_argument(&resp, "file", "MISSING_FILE");
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn a_file_part_without_a_content_type_is_400_missing_content_type() {
    let (env, user, chat) = env_user_chat().await;
    let part = Part {
        name: "file",
        filename: Some("a.pdf"),
        content_type: None,
        data: b"data",
    };

    let resp = upload_parts(&env, &user, &chat, &[part]).await;

    assert_invalid_argument(&resp, "content_type", "MISSING_CONTENT_TYPE");
    assert!(calls(&env).is_empty());
}

// ===========================================================================
// Provider and storage failures
// ===========================================================================

#[tokio::test]
async fn a_failed_provider_file_upload_is_503_and_leaves_a_failed_row() {
    for status in [400, 401, 429, 500, 502] {
        let (env, user, chat) = env_user_chat().await;
        env.provider().enqueue("/v1/files", provider_error(status));

        let resp = upload_pdf(&env, &user, &chat).await;

        assert_unavailable(&resp, 10);
        assert_eq!(
            call_lines(&env),
            vec!["POST /llm.test/v1/files".to_owned()],
            "provider status {status}: the write is not retried"
        );
        let rows = chat_rows(&env, &chat).await;
        assert_eq!(rows.len(), 1, "provider status {status}");
        assert_eq!(rows[0].status, "failed");
        assert!(
            rows[0]
                .error_code
                .as_deref()
                .is_some_and(|code| !code.is_empty()),
            "provider status {status}: the failed row names its error code"
        );
        assert!(rows[0].provider_file_id.is_none());
    }
}

#[tokio::test]
async fn a_failed_upload_stays_visible_via_get_with_its_error_code() {
    let (env, user, chat) = env_user_chat().await;
    env.provider().enqueue("/v1/files", provider_error(500));
    assert_unavailable(&upload_pdf(&env, &user, &chat).await, 10);
    let failed = chat_rows(&env, &chat).await.remove(0);

    let resp = get_attachment(&env, &user, &chat, &failed.id.to_string()).await;

    expect_status(&resp, StatusCode::OK);
    let detail = resp.json();
    let mut keys = DOCUMENT_DETAIL_KEYS.to_vec();
    keys.push("error_code");
    assert_keys(&detail, &keys);
    assert_eq!(detail["status"], json!("failed"));
    assert_eq!(detail["error_code"], json!(failed.error_code.unwrap()));
    assert_no_provider_ids(&detail.to_string());
}

#[tokio::test]
async fn a_failed_vector_store_create_is_503_and_removes_the_placeholder() {
    let (env, user, chat) = env_user_chat().await;
    script_file(&env, "file-doc1");
    env.provider()
        .enqueue("/v1/vector_stores", provider_error(500));

    let resp = upload_pdf(&env, &user, &chat).await;

    assert_unavailable(&resp, 10);
    assert!(
        store_rows(&env, &chat).await.is_empty(),
        "the placeholder row is deleted (best effort) so that a later upload can create the store"
    );
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert!(rows[0].error_code.is_some());
    assert_eq!(
        calls_to(&env, "POST", &vector_stores_path()).len(),
        1,
        "no retry"
    );
    // A later upload creates the store.
    upload_ready_document(&env, &user, &chat, "b.pdf", "file-doc2", "vs_chat1", true).await;
    assert_eq!(store_rows(&env, &chat).await.len(), 1);
}

#[tokio::test]
async fn a_failed_add_to_the_vector_store_is_503_with_indexing_failed_and_deletes_the_provider_file()
 {
    let (env, user, chat) = env_user_chat().await;
    script_file(&env, "file-doc1");
    script_store_create(&env, "vs_chat1");
    env.provider()
        .enqueue("/vector_stores/vs_chat1/files", provider_error(500));

    let resp = upload_pdf(&env, &user, &chat).await;

    let problem = assert_unavailable(&resp, 10);
    assert!(
        !problem.to_string().contains("indexing_failed"),
        "indexing_failed is not in the response body: {problem}"
    );
    let rows = chat_rows(&env, &chat).await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
    wait_calls(&env, "DELETE", &file_path("file-doc1"), 1).await;
    assert_eq!(
        calls_to(&env, "POST", &vector_store_files_path("vs_chat1")).len(),
        1,
        "the add is not retried"
    );
    assert_eq!(calls_to(&env, "POST", &files_path()).len(), 1);
}

#[tokio::test]
async fn uploads_beyond_the_concurrency_limit_are_503_with_retry_after_5() {
    let env = env_with_config(json!({ "rag": { "max_concurrent_uploads": 1 } })).await;
    let user = TestUser::new();
    let busy_chat = new_chat(&env, &user).await;
    let other_chat = new_chat(&env, &user).await;
    // The provider never finishes the first file upload, so it holds the only upload slot.
    env.provider()
        .enqueue("/v1/files", ScriptedResponse::SseThenHang(Vec::new()));
    let first = upload_pdf(&env, &user, &busy_chat);
    tokio::pin!(first);
    let second = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        upload_pdf(&env, &user, &other_chat).await
    };

    let resp = tokio::select! {
        _ = &mut first => panic!("the first upload waits for the provider"),
        resp = second => resp,
    };

    assert_unavailable(&resp, 5);
    assert_eq!(
        calls_to(&env, "POST", &files_path()).len(),
        1,
        "the rejected upload never reached the provider"
    );
}

// ===========================================================================
// GET
// ===========================================================================

#[tokio::test]
async fn get_of_an_unknown_attachment_is_404_attachment() {
    let (env, user, chat) = env_user_chat().await;

    let resp = get_attachment(&env, &user, &chat, &Uuid::new_v4().to_string()).await;

    assert_attachment_not_found(&resp);
}

#[tokio::test]
async fn get_of_a_non_uuid_attachment_id_is_400() {
    let (env, user, chat) = env_user_chat().await;

    let resp = get_attachment(&env, &user, &chat, "not-a-uuid").await;

    assert_invalid_argument(&resp, "path", "invalid_path_params");
}

#[tokio::test]
async fn get_hides_attachments_of_other_uploaders_other_chats_and_deleted_rows() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    let colleague = TestUser::in_tenant(user.tenant_id);
    let uploaded_by_other = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-1")
        .uploaded_by(colleague.user_id)
        .insert(&env)
        .await;
    let in_other_chat = RowSeed::new(&user, &other_chat)
        .status("ready")
        .provider_file("file-2")
        .insert(&env)
        .await;
    let soft_deleted = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-3")
        .deleted()
        .insert(&env)
        .await;

    for id in [uploaded_by_other, in_other_chat, soft_deleted] {
        let resp = get_attachment(&env, &user, &chat, &id.to_string()).await;

        assert_attachment_not_found(&resp);
    }
}

#[tokio::test]
async fn get_reports_every_status_without_provider_identifiers() {
    let (env, user, chat) = env_user_chat().await;
    for (status, error_code) in [
        ("pending", None),
        ("uploaded", None),
        ("ready", None),
        ("failed", Some("upload_abandoned")),
    ] {
        let mut seed = RowSeed::new(&user, &chat)
            .status(status)
            .provider_file("file-secret-1")
            .filename("n.txt");
        if let Some(code) = error_code {
            seed = seed.error_code(code);
        }
        let id = seed.insert(&env).await;

        let resp = get_attachment(&env, &user, &chat, &id.to_string()).await;

        expect_status(&resp, StatusCode::OK);
        let detail = resp.json();
        assert_eq!(detail["status"], json!(status));
        assert_eq!(detail["error_code"].as_str(), error_code, "{detail}");
        assert!(detail.get("img_thumbnail").is_none() && detail.get("doc_summary").is_none());
        assert_no_provider_ids(&detail.to_string());
    }
}

#[tokio::test]
async fn the_thumbnail_is_present_only_for_ready_images() {
    let (env, user, chat) = env_user_chat().await;
    let bytes = [1_u8, 2, 3, 4, 5];
    let ready_image = RowSeed::new(&user, &chat)
        .image()
        .status("ready")
        .provider_file("file-1")
        .thumbnail(&bytes, 12, 8)
        .insert(&env)
        .await;
    let uploaded_image = RowSeed::new(&user, &chat)
        .image()
        .status("uploaded")
        .provider_file("file-2")
        .thumbnail(&bytes, 12, 8)
        .insert(&env)
        .await;
    let ready_document = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-3")
        .thumbnail(&bytes, 12, 8)
        .insert(&env)
        .await;

    let ready = get_attachment(&env, &user, &chat, &ready_image.to_string())
        .await
        .json();
    let uploaded = get_attachment(&env, &user, &chat, &uploaded_image.to_string())
        .await
        .json();
    let document = get_attachment(&env, &user, &chat, &ready_document.to_string())
        .await
        .json();

    assert_eq!(
        ready["img_thumbnail"],
        json!({
            "content_type": "image/webp", "width": 12, "height": 8,
            "data_base64": "AQIDBAU="
        })
    );
    assert!(uploaded.get("img_thumbnail").is_none(), "{uploaded}");
    assert!(document.get("img_thumbnail").is_none(), "{document}");
}

// ===========================================================================
// DELETE
// ===========================================================================

#[tokio::test]
async fn delete_soft_deletes_the_attachment_and_enqueues_the_cleanup_message() {
    let (env, user, chat) = env_user_chat().await;
    let detail =
        upload_ready_document(&env, &user, &chat, "a.pdf", "file-doc1", "vs_chat1", true).await;
    let id = detail_id(&detail);

    let resp = delete_attachment(&env, &user, &chat, &id.to_string()).await;

    expect_status(&resp, StatusCode::NO_CONTENT);
    assert!(resp.body.is_empty());
    let stored = row(&env, id).await;
    assert!(stored.deleted_at.is_some(), "soft-deleted");
    assert_eq!(stored.cleanup_status.as_deref(), Some("pending"));
    let payloads = env
        .outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
        .await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let payload = &payloads[0];
    assert_eq!(payload["event_type"], json!("attachment_deleted"));
    assert_eq!(payload["tenant_id"], json!(user.tenant_id));
    assert_eq!(payload["chat_id"], json!(chat));
    assert_eq!(payload["attachment_id"], json!(id.to_string()));
    assert_eq!(payload["provider_file_id"], json!("file-doc1"));
    assert!(
        payload["vector_store_id"].is_null(),
        "always null in P1: {payload}"
    );
    assert_eq!(payload["storage_backend"], json!("openai"));
    assert_eq!(payload["attachment_kind"], json!("document"));
    assert_timestamp(&payload["deleted_at"]);
    assert!(payload["secondary_ref"].is_null(), "{payload}");
    let keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for key in keys {
        assert!(
            [
                "event_type",
                "tenant_id",
                "chat_id",
                "attachment_id",
                "provider_file_id",
                "vector_store_id",
                "storage_backend",
                "attachment_kind",
                "deleted_at",
                "secondary_ref"
            ]
            .contains(&key),
            "unexpected payload member `{key}`"
        );
    }
}

#[tokio::test]
async fn a_deleted_attachment_disappears_from_get() {
    let (env, user, chat) = env_user_chat().await;
    let id = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);

    delete_ok(&env, &user, &chat, id).await;

    assert_attachment_not_found(&get_attachment(&env, &user, &chat, &id.to_string()).await);
    assert_eq!(
        env.outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
            .await[0]["attachment_kind"],
        json!("image")
    );
}

#[tokio::test]
async fn a_repeated_delete_is_204_without_a_duplicate_event() {
    let (env, user, chat) = env_user_chat().await;
    let id = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);
    delete_ok(&env, &user, &chat, id).await;

    for _ in 0..2 {
        delete_ok(&env, &user, &chat, id).await;
    }

    assert_eq!(
        env.outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
            .await
            .len(),
        1,
        "exactly one cleanup event"
    );
}

#[tokio::test]
async fn deleting_an_attachment_without_a_provider_file_enqueues_a_null_provider_file_id() {
    let (env, user, chat) = env_user_chat().await;
    env.provider().enqueue("/v1/files", provider_error(500));
    assert_unavailable(&upload_pdf(&env, &user, &chat).await, 10);
    let failed = chat_rows(&env, &chat).await.remove(0);

    delete_ok(&env, &user, &chat, failed.id).await;

    let payloads = env
        .outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
        .await;
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0]["provider_file_id"].is_null(), "{}", payloads[0]);
}

#[tokio::test]
async fn deleting_an_attachment_of_a_submitted_message_is_409_attachment_locked() {
    let (env, user, chat) = env_user_chat().await;
    let id = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);
    let sent = send(
        &env,
        &user,
        &chat,
        json!({ "content": "look", "attachment_ids": [id.to_string()] }),
    )
    .await;
    expect_stream(&sent);

    let resp = delete_attachment(&env, &user, &chat, &id.to_string()).await;

    let problem = assert_problem(&resp, 409, Some("already_exists"));
    assert_eq!(
        problem["context"]["resource_name"],
        json!("attachment_locked")
    );
    assert!(
        row(&env, id).await.deleted_at.is_none(),
        "the attachment is untouched"
    );
    assert!(
        env.outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
    expect_status(
        &get_attachment(&env, &user, &chat, &id.to_string()).await,
        StatusCode::OK,
    );
}

#[tokio::test]
async fn an_attachment_that_is_not_referenced_can_be_deleted_even_after_other_turns() {
    let (env, user, chat) = env_user_chat().await;
    let sent_image = detail_id(&upload_ready_image(&env, &user, &chat, "a.png", "file-img1").await);
    let spare_image =
        detail_id(&upload_ready_image(&env, &user, &chat, "b.png", "file-img2").await);
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "look", "attachment_ids": [sent_image.to_string()] }),
        )
        .await,
    );

    delete_ok(&env, &user, &chat, spare_image).await;

    assert!(row(&env, spare_image).await.deleted_at.is_some());
    assert!(row(&env, sent_image).await.deleted_at.is_none());
}

#[tokio::test]
async fn delete_of_unknown_other_chat_and_other_uploader_attachments_is_404() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    let colleague = TestUser::in_tenant(user.tenant_id);
    let by_other = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-1")
        .uploaded_by(colleague.user_id)
        .insert(&env)
        .await;
    let by_other_deleted = RowSeed::new(&user, &chat)
        .status("ready")
        .provider_file("file-2")
        .uploaded_by(colleague.user_id)
        .deleted()
        .insert(&env)
        .await;
    let in_other_chat = RowSeed::new(&user, &other_chat)
        .status("ready")
        .provider_file("file-3")
        .insert(&env)
        .await;

    for id in [Uuid::new_v4(), by_other, by_other_deleted, in_other_chat] {
        let resp = delete_attachment(&env, &user, &chat, &id.to_string()).await;

        assert_attachment_not_found(&resp);
    }
    assert!(
        row(&env, by_other).await.deleted_at.is_none(),
        "another user's attachment is untouched"
    );
    assert!(row(&env, in_other_chat).await.deleted_at.is_none());
    assert!(
        env.outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
            .await
            .is_empty()
    );
}

// ===========================================================================
// Tenant and owner isolation, authorization
// ===========================================================================

#[tokio::test]
async fn foreign_users_and_tenants_cannot_touch_attachments() {
    for mode in [AuthzMode::OwnerScoped, AuthzMode::TenantOnly] {
        let env = TestEnvBuilder::new().authz(mode).build().await;
        let owner = TestUser::new();
        let chat = new_chat(&env, &owner).await;
        let id = detail_id(
            &upload_ready_document(&env, &owner, &chat, "a.pdf", "file-doc1", "vs_1", true).await,
        );
        let calls_before = call_lines(&env).len();

        for intruder in [TestUser::in_tenant(owner.tenant_id), TestUser::new()] {
            let uploaded = env
                .request_raw(
                    &intruder,
                    upload_request_raw(
                        &chat,
                        Some(&multipart_content_type()),
                        multipart_body(&[Part::file("x.pdf", PDF, b"data")]),
                    ),
                )
                .await;
            let fetched = get_attachment(&env, &intruder, &chat, &id.to_string()).await;
            let deleted = delete_attachment(&env, &intruder, &chat, &id.to_string()).await;

            for resp in [uploaded, fetched, deleted] {
                assert_problem(&resp, 404, Some("not_found"));
                assert_no_provider_ids(&String::from_utf8_lossy(&resp.body));
            }
        }

        assert!(row(&env, id).await.deleted_at.is_none());
        assert_eq!(
            chat_rows(&env, &chat).await.len(),
            1,
            "no attachment was created"
        );
        assert_eq!(call_lines(&env).len(), calls_before, "no provider call");
        assert!(
            env.outbox_payloads(attachments::ATTACHMENT_CLEANUP_QUEUE)
                .await
                .is_empty()
        );
    }
}

#[tokio::test]
async fn attachment_routes_are_denied_when_the_pdp_denies() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;
    let user = TestUser::new();
    let chat = Uuid::new_v4().to_string();
    let attachment = Uuid::new_v4().to_string();

    let uploaded = env
        .request_raw(
            &user,
            upload_request_raw(
                &chat,
                Some(&multipart_content_type()),
                multipart_body(&[Part::file("x.pdf", PDF, b"data")]),
            ),
        )
        .await;
    let fetched = get_attachment(&env, &user, &chat, &attachment).await;
    let deleted = delete_attachment(&env, &user, &chat, &attachment).await;

    for resp in [uploaded, fetched, deleted] {
        let problem = assert_problem(&resp, 403, Some("permission_denied"));
        assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    }
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn attachment_routes_are_503_when_the_pdp_is_unavailable() {
    let env = TestEnvBuilder::new()
        .authz(AuthzMode::Unavailable)
        .build()
        .await;
    let user = TestUser::new();
    let chat = Uuid::new_v4().to_string();
    let attachment = Uuid::new_v4().to_string();

    let fetched = get_attachment(&env, &user, &chat, &attachment).await;
    let deleted = delete_attachment(&env, &user, &chat, &attachment).await;

    assert_unavailable(&fetched, 5);
    assert_unavailable(&deleted, 5);
}

// ===========================================================================
// Anthropic chats (ADR-0005: file storage through the rag provider, secondary image copy)
// ===========================================================================

async fn anthropic_env(thumbnail: Value) -> (TestEnv, TestUser, String) {
    let claude = merge(
        catalog_entry("claude", "Premium", true, true),
        json!({ "provider_id": "anthropic" }),
    );
    let env = TestEnvBuilder::new()
        .gear_config(json!({
            "thumbnail": thumbnail,
            "providers": { "anthropic": {
                "kind": "anthropic_messages",
                "host": "anthropic.test",
                "api_path": "/v1/messages",
                "storage_kind": "openai",
                "rag_provider": "openai",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {
                    "header": "x-api-key", "prefix": "", "secret_ref": "cred://anthropic-key"
                }
            } }
        }))
        .policy_config(policy(vec![
            claude,
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
        ]))
        .build()
        .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, "claude").await;
    (env, user, chat)
}

#[tokio::test]
async fn an_image_in_an_anthropic_chat_also_gets_a_secondary_copy() {
    let (env, user, chat) = anthropic_env(json!({})).await;
    script_plain_file(&env, "file-primary");
    env.provider().enqueue(
        "/anthropic.test/",
        attachments::ok(json!({ "id": "file_011secondary", "type": "file" })),
    );

    let detail = expect_created(&upload(&env, &user, &chat, "a.png", PNG, &png(64, 64)).await);

    let stored = row(&env, detail_id(&detail)).await;
    assert_eq!(stored.status, "ready");
    assert_eq!(stored.provider_file_id.as_deref(), Some("file-primary"));
    assert_eq!(stored.secondary_status, "uploaded");
    assert_eq!(stored.secondary_provider_kind.as_deref(), Some("anthropic"));
    assert_eq!(
        stored.secondary_file_id.as_deref(),
        Some("file_011secondary")
    );
    assert_eq!(
        calls_to(&env, "POST", &files_path()).len(),
        1,
        "the primary copy goes to the rag provider"
    );
    assert!(
        calls(&env)
            .iter()
            .any(|call| call.method == "POST" && call.path.starts_with("/anthropic.test/")),
        "the secondary copy goes to the Anthropic Files API: {:?}",
        call_lines(&env)
    );
    assert_no_provider_ids(&detail.to_string());
}

#[tokio::test]
async fn documents_in_an_anthropic_chat_get_no_secondary_copy() {
    let (env, user, chat) = anthropic_env(json!({})).await;
    script_document(&env, "file-doc1", "vs_chat1", true);

    let detail = expect_created(&upload_pdf(&env, &user, &chat).await);

    let stored = row(&env, detail_id(&detail)).await;
    assert_eq!(stored.secondary_status, "not_attempted");
    assert!(stored.secondary_file_id.is_none() && stored.secondary_provider_kind.is_none());
    assert!(
        calls(&env)
            .iter()
            .all(|call| !call.path.starts_with("/anthropic.test/")),
        "documents are indexed in the rag provider only"
    );
}

#[tokio::test]
async fn an_image_above_the_decode_cap_gets_no_secondary_copy() {
    let (env, user, chat) = anthropic_env(json!({ "max_decode_bytes": 200 })).await;
    let image = png(64, 64);
    assert!(image.len() > 200);
    script_plain_file(&env, "file-primary");

    let detail = expect_created(&upload(&env, &user, &chat, "a.png", PNG, &image).await);

    let stored = row(&env, detail_id(&detail)).await;
    assert_eq!(stored.status, "ready");
    assert_eq!(stored.secondary_status, "not_attempted");
    assert!(
        calls(&env)
            .iter()
            .all(|call| !call.path.starts_with("/anthropic.test/"))
    );
}

#[tokio::test]
async fn validation_errors_before_the_insert_leave_no_attachment_row() {
    let (env, user, chat) = env_user_chat().await;

    let unsupported = upload(&env, &user, &chat, "a.zip", "application/zip", b"x").await;
    let no_file = upload_parts(&env, &user, &chat, &[Part::field("note", "x")]).await;

    assert_invalid_argument(&unsupported, "content_type", "UNSUPPORTED_CONTENT_TYPE");
    assert_invalid_argument(&no_file, "file", "MISSING_FILE");
    assert_eq!(
        row_count(&env).await,
        0,
        "validation errors create no attachment row"
    );
    assert!(calls(&env).is_empty());
}
