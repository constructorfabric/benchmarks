#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::case_sensitive_file_extension_comparisons
)]
//! Slice 1: the upload endpoint up to the pending row
//! (`cpt-cf-mini-chat-dod-attachments-upload-endpoint`,
//! `cpt-cf-mini-chat-dod-attachments-error-contract`): the check order, the
//! multipart reasons, classification, size, kill switches, per-chat limits, the
//! concurrency permit, and what is stored for an accepted file.
//!
//! A rejected upload never stores a row and never calls the provider; an
//! accepted one inserts the `pending` row before the provider is called, which is
//! what the "accepted" tests read (the fake provider is not scripted here, so the
//! request ends at the provider step).

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::attachment_test_env::{
    ATTACHMENT_TYPE, AttEnv, AttOptions, AttSnap, BOUNDARY, counting_body, file_part, held_body,
    multipart_body, multipart_content_type, multipart_head, multipart_tail, text_part,
};
use crate::chat_test_env::{CHAT_PEP_TYPE, CHAT_TYPE, Reply, ts};
use crate::config::RagConfig;
use crate::domain::service::quota::test_support::{NO_SWITCHES, PolicyStep};
use crate::test_doubles::{
    PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx, user_ctx,
};
use mini_chat_sdk::KillSwitches;

const PDF: &str = "application/pdf";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

async fn setup() -> (AttEnv, Uuid) {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    (env, chat)
}

fn rag(edit: impl FnOnce(&mut RagConfig)) -> AttOptions {
    let mut options = AttOptions::default();
    edit(&mut options.rag);
    options
}

/// The attachment row an upload inserted (the provider call that follows fails).
async fn new_row(
    env: &AttEnv,
    chat: Uuid,
    name: &str,
    content_type: &str,
    bytes: &[u8],
) -> AttSnap {
    let before = env.att_ids().await;
    let _ = env.upload(chat, name, content_type, bytes).await;
    let after = env.att_ids().await;
    let added: Vec<Uuid> = after
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(
        added.len(),
        1,
        "{name} / {content_type}: exactly one row was inserted"
    );
    env.att(added[0]).await
}

async fn assert_untouched(env: &AttEnv) {
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "no provider request: {:?}",
        env.sequence()
    );
    assert_eq!(env.att_count().await, 0, "no attachments row");
}

fn assert_attachment_problem(reply: &Reply, status: u16, field: &str, reason: &str) {
    assert_eq!(reply.status.as_u16(), status, "{}", reply.text);
    assert_eq!(
        reply.first_violation(),
        (field.to_owned(), reason.to_owned()),
        "{}",
        reply.text
    );
    assert_eq!(reply.resource_type(), ATTACHMENT_TYPE, "{}", reply.text);
}

// ================================================================================================
// Authorization, chat, model: all before the body is read
// ================================================================================================

#[tokio::test]
async fn the_upload_asks_the_pdp_for_upload_attachment_on_the_chat() {
    let (env, chat) = setup().await;

    let _ = env.upload(chat, "a.zip", "application/zip", b"x").await;

    let request = env.stream.quota.pdp.requests();
    assert_eq!(request.len(), 1, "{request:?}");
    assert_eq!(request[0].action.name, "upload_attachment");
    assert_eq!(request[0].resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request[0].resource.id, Some(chat));
    assert!(request[0].context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_is_403_before_the_body_is_read() {
    let env = AttEnv::with(AttOptions {
        pdp: PdpReply::Deny,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let (body, polls) = counting_body(vec![multipart_body(
        BOUNDARY,
        &[file_part("a.pdf", PDF, b"x")],
    )]);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            body,
        )
        .await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");
    assert_eq!(polls.load(Ordering::SeqCst), 0, "no body byte was read");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_5_before_the_body_is_read() {
    let env = AttEnv::with(AttOptions {
        pdp: PdpReply::Failure,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let (body, polls) = counting_body(vec![multipart_body(
        BOUNDARY,
        &[file_part("a.pdf", PDF, b"x")],
    )]);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            body,
        )
        .await;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_404_with_the_chat_type_before_the_body_is_read() {
    let env = AttEnv::new().await;
    let own = env.chat("m-oa").await;
    let deleted = env.chat("m-oa").await;
    env.delete_chat(deleted).await;
    let foreign_user = env
        .chat_of(USER_TENANT, crate::test_doubles::OTHER_USER, "m-oa")
        .await;
    let foreign_tenant = env
        .chat_of(
            crate::test_doubles::OTHER_TENANT,
            crate::test_doubles::OTHER_USER,
            "m-oa",
        )
        .await;

    for (label, ctx, chat) in [
        ("unknown", user_ctx(), Uuid::new_v4()),
        ("soft-deleted", user_ctx(), deleted),
        ("another user's", user_ctx(), foreign_user),
        ("another tenant's", user_ctx(), foreign_tenant),
        ("mine, asked by another user", other_user_ctx(), own),
        ("mine, asked by another tenant", other_tenant_ctx(), own),
    ] {
        let (body, polls) = counting_body(vec![multipart_body(
            BOUNDARY,
            &[file_part("a.pdf", PDF, b"x")],
        )]);

        let reply = env
            .upload_raw(&ctx, chat, Some(&multipart_content_type(BOUNDARY)), body)
            .await;

        reply.assert_not_found(CHAT_TYPE);
        assert_eq!(
            polls.load(Ordering::SeqCst),
            0,
            "{label}: no body byte was read"
        );
    }
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_chat_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = AttEnv::new().await;

    let reply = env
        .send(
            "POST",
            "/mini-chat/v1/chats/not-a-uuid/attachments",
            &user_ctx(),
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(multipart_body(BOUNDARY, &[file_part("a.pdf", PDF, b"x")])),
        )
        .await;

    reply.assert_invalid("path", "invalid_path_params");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_chat_whose_model_left_the_catalog_is_400_invalid_model_before_the_body_is_read() {
    let env = AttEnv::new().await;
    let chat = env.chat("retired-model").await;
    let (body, polls) = counting_body(vec![multipart_body(
        BOUNDARY,
        &[file_part("a.pdf", PDF, b"x")],
    )]);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            body,
        )
        .await;

    reply.assert_invalid("model", "INVALID_MODEL");
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_disabled_model_still_accepts_uploads() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-off").await;

    let reply = env.upload(chat, "a.zip", "application/zip", b"x").await;

    assert_attachment_problem(&reply, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
}

#[tokio::test]
async fn an_unreadable_policy_is_500_internal_before_the_body_is_read() {
    for step in [PolicyStep::Version, PolicyStep::Snapshot] {
        let (env, chat) = setup().await;
        env.stream.quota.policy.fail(step);
        let (body, polls) = counting_body(vec![multipart_body(
            BOUNDARY,
            &[file_part("a.pdf", PDF, b"x")],
        )]);

        let reply = env
            .upload_raw(
                &user_ctx(),
                chat,
                Some(&multipart_content_type(BOUNDARY)),
                body,
            )
            .await;

        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{step:?}: {}",
            reply.text
        );
        assert!(reply.category().contains("internal"));
        assert_eq!(polls.load(Ordering::SeqCst), 0, "{step:?}");
        assert_untouched(&env).await;
    }
}

#[tokio::test]
async fn the_policy_is_read_for_every_upload() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let png = crate::attachment_test_env::png_bytes(4, 4);

    let first = new_row(&env, chat, "a.png", "image/png", &png).await;
    env.set_switches(KillSwitches {
        disable_images: true,
        ..NO_SWITCHES
    });
    let second = env.upload(chat, "b.png", "image/png", &png).await;

    assert_eq!(first.kind, "image");
    assert_eq!(second.status, StatusCode::BAD_REQUEST, "{}", second.text);
    assert_eq!(env.att_count().await, 1);
}

// ================================================================================================
// Multipart reasons
// ================================================================================================

#[tokio::test]
async fn a_request_without_a_boundary_is_400_boundary_required_on_content_type() {
    let (env, chat) = setup().await;
    let body = multipart_body(BOUNDARY, &[file_part("a.pdf", PDF, b"x")]);

    for content_type in [None, Some("multipart/form-data"), Some("application/json")] {
        let reply = env
            .upload_raw(&user_ctx(), chat, content_type, Body::from(body.clone()))
            .await;

        assert_attachment_problem(&reply, 400, "content_type", "BOUNDARY_REQUIRED");
    }
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_truncated_body_is_400_multipart_error_on_multipart() {
    let (env, chat) = setup().await;
    let mut body = multipart_head("a.pdf", PDF, b"%PDF-1.7 hello");
    body.truncate(body.len() - 4);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;

    assert_attachment_problem(&reply, 400, "multipart", "MULTIPART_ERROR");
    let header_cut = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"fi");
    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(header_cut),
        )
        .await;
    assert_attachment_problem(&reply, 400, "multipart", "MULTIPART_ERROR");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_body_without_a_file_part_is_400_missing_file_on_file() {
    let (env, chat) = setup().await;
    let body = multipart_body(
        BOUNDARY,
        &[text_part("purpose", "assistants"), text_part("note", "hi")],
    );

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;

    assert_attachment_problem(&reply, 400, "file", "MISSING_FILE");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_file_part_without_a_content_type_is_400_missing_content_type_on_content_type() {
    let (env, chat) = setup().await;
    let mut part = file_part("a.pdf", PDF, b"x");
    part.content_type = None;
    let body = multipart_body(BOUNDARY, &[part]);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;

    assert_attachment_problem(&reply, 400, "content_type", "MISSING_CONTENT_TYPE");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn the_first_file_part_is_used_and_other_parts_are_ignored() {
    let (env, chat) = setup().await;
    let body = multipart_body(
        BOUNDARY,
        &[
            text_part("purpose", "assistants"),
            file_part("first.pdf", PDF, b"one"),
            file_part("second.txt", "text/plain", b"two"),
        ],
    );

    let _ = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;

    let (_, row) = env.only_att().await;
    assert_eq!(row.filename, "first.pdf");
    assert_eq!(row.size_bytes, 3);
}

// ================================================================================================
// Classification
// ================================================================================================

#[tokio::test]
async fn unsupported_types_are_400_unsupported_content_type_on_file() {
    let (env, chat) = setup().await;

    for (name, content_type) in [
        ("a.zip", "application/zip"),
        ("a.bin", "application/octet-stream"),
        ("a.zip", "application/octet-stream"),
        ("noextension", "application/octet-stream"),
        ("a.svg", "image/svg+xml"),
        ("a.csv.exe", "application/x-msdownload"),
    ] {
        let reply = env.upload(chat, name, content_type, b"x").await;

        assert_attachment_problem(&reply, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
    }
    assert_untouched(&env).await;
}

#[tokio::test]
async fn octet_stream_with_a_known_extension_is_accepted_as_that_type() {
    let (env, chat) = setup().await;

    let docx = new_row(&env, chat, "report.DOCX", "application/octet-stream", b"PK").await;
    let md = new_row(
        &env,
        chat,
        "notes.markdown",
        "application/octet-stream",
        b"# hi",
    )
    .await;

    assert_eq!(docx.content_type, DOCX);
    assert_eq!(md.content_type, "text/markdown");
}

#[tokio::test]
async fn aliases_are_stored_in_their_first_form_and_parameters_are_dropped() {
    let (env, chat) = setup().await;

    let jpg = new_row(
        &env,
        chat,
        "a.jpg",
        "image/jpg",
        &crate::attachment_test_env::png_bytes(4, 4),
    )
    .await;
    let py = new_row(
        &env,
        chat,
        "a.py",
        "Text/X-Script.Python; charset=utf-8",
        b"print()",
    )
    .await;

    assert_eq!(jpg.content_type, "image/jpeg");
    assert_eq!(py.content_type, "text/x-python");
}

#[tokio::test]
async fn csv_is_stored_as_plain_text() {
    let (env, chat) = setup().await;

    let declared = new_row(&env, chat, "data.csv", "text/csv", b"a,b\n1,2").await;
    let inferred = new_row(&env, chat, "data2.csv", "application/octet-stream", b"a,b").await;

    assert_eq!(declared.content_type, "text/plain");
    assert_eq!(inferred.content_type, "text/plain");
    assert!(declared.for_file_search);
}

#[tokio::test]
async fn csv_is_rejected_when_csv_upload_is_disabled() {
    let env = AttEnv::with(rag(|r| r.allow_csv_upload = false)).await;
    let chat = env.chat("m-oa").await;

    let reply = env.upload(chat, "data.csv", "text/csv", b"a,b").await;
    let inferred = env
        .upload(chat, "data.csv", "application/octet-stream", b"a,b")
        .await;

    assert_attachment_problem(&reply, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
    assert_attachment_problem(&inferred, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
    assert_untouched(&env).await;
}

#[tokio::test]
async fn kind_and_purposes_follow_the_type() {
    let (env, chat) = setup().await;
    let png = crate::attachment_test_env::png_bytes(4, 4);

    let pdf = new_row(&env, chat, "a.pdf", PDF, b"%PDF").await;
    let sheet = new_row(&env, chat, "a.xlsx", XLSX, b"PK").await;
    let image = new_row(&env, chat, "a.png", "image/png", &png).await;

    assert_eq!(
        (
            pdf.kind.as_str(),
            pdf.for_file_search,
            pdf.for_code_interpreter
        ),
        ("document", true, false)
    );
    assert_eq!(
        (
            sheet.kind.as_str(),
            sheet.for_file_search,
            sheet.for_code_interpreter
        ),
        ("document", false, true)
    );
    assert_eq!(
        (
            image.kind.as_str(),
            image.for_file_search,
            image.for_code_interpreter
        ),
        ("image", false, false)
    );
}

#[tokio::test]
async fn images_under_the_kill_switch_are_400_feature_disabled_on_images() {
    let env = AttEnv::with(AttOptions {
        switches: KillSwitches {
            disable_images: true,
            ..NO_SWITCHES
        },
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;

    let reply = env
        .upload(
            chat,
            "a.png",
            "image/png",
            &crate::attachment_test_env::png_bytes(4, 4),
        )
        .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.category().contains("failed_precondition"),
        "{}",
        reply.text
    );
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(violation["subject"], "images");
    assert_eq!(violation["type"], "FEATURE_DISABLED");
    assert_untouched(&env).await;
    let doc = new_row(&env, chat, "a.pdf", PDF, b"%PDF").await;
    assert_eq!(doc.kind, "document", "documents are unaffected");
}

#[tokio::test]
async fn a_spreadsheet_without_the_code_interpreter_is_400_code_interpreter_unavailable() {
    let switched = AttEnv::with(AttOptions {
        switches: KillSwitches {
            disable_code_interpreter: true,
            ..NO_SWITCHES
        },
        ..AttOptions::default()
    })
    .await;
    let chat = switched.chat("m-oa").await;
    let unsupported = AttEnv::new().await;
    let no_ci = unsupported.chat("m-noci").await;

    let by_switch = switched.upload(chat, "a.xlsx", XLSX, b"PK").await;
    let by_model = unsupported.upload(no_ci, "a.xlsx", XLSX, b"PK").await;

    assert_attachment_problem(&by_switch, 400, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_attachment_problem(&by_model, 400, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_untouched(&switched).await;
    assert_untouched(&unsupported).await;
}

#[tokio::test]
async fn other_uploads_do_not_need_the_code_interpreter() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-noci").await;

    let pdf = new_row(&env, chat, "a.pdf", PDF, b"%PDF").await;
    let image = new_row(
        &env,
        chat,
        "a.png",
        "image/png",
        &crate::attachment_test_env::png_bytes(4, 4),
    )
    .await;

    assert!(pdf.for_file_search);
    assert_eq!(image.kind, "image");
}

#[tokio::test]
async fn classification_comes_before_the_size_check() {
    let env = AttEnv::with(rag(|r| r.uploaded_file_max_size_kb = 1)).await;
    let chat = env.chat("m-oa").await;

    let reply = env
        .upload(chat, "big.zip", "application/zip", &vec![0_u8; 8192])
        .await;

    assert_attachment_problem(&reply, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
}

// ================================================================================================
// Size
// ================================================================================================

#[tokio::test]
async fn a_document_over_the_configured_limit_is_400_file_too_large_on_content_length() {
    let env = AttEnv::with(rag(|r| r.uploaded_file_max_size_kb = 10)).await;
    let chat = env.chat("m-oa").await;

    let at_limit = new_row(&env, chat, "ok.pdf", PDF, &vec![b'x'; 10 * 1024]).await;
    let over = env
        .upload(chat, "big.pdf", PDF, &vec![b'x'; 10 * 1024 + 1])
        .await;

    assert_eq!(
        at_limit.size_bytes,
        10 * 1024,
        "exactly at the limit is accepted"
    );
    assert_attachment_problem(&over, 400, "content_length", "FILE_TOO_LARGE");
    assert!(over.category().contains("out_of_range"));
    assert_eq!(env.att_count().await, 1, "the oversized upload left no row");
}

#[tokio::test]
async fn an_image_over_the_image_limit_is_400_file_too_large() {
    let env = AttEnv::with(rag(|r| {
        r.uploaded_image_max_size_kb = 4;
        r.uploaded_file_max_size_kb = 64;
    }))
    .await;
    let chat = env.chat("m-oa").await;

    let image = env
        .upload(chat, "big.png", "image/png", &vec![0_u8; 4 * 1024 + 1])
        .await;
    let document = new_row(&env, chat, "ok.pdf", PDF, &vec![b'x'; 8 * 1024]).await;

    assert_attachment_problem(&image, 400, "content_length", "FILE_TOO_LARGE");
    assert_eq!(
        document.size_bytes,
        8 * 1024,
        "the document limit is its own"
    );
}

#[tokio::test]
async fn a_smaller_model_limit_lowers_the_effective_limit() {
    let env = AttEnv::new().await;
    let small = env.chat("m-small").await;
    let normal = env.chat("m-oa").await;
    let mib = 1_048_576;

    let at_limit = new_row(&env, small, "ok.pdf", PDF, &vec![b'x'; mib]).await;
    let over = env
        .upload(small, "big.pdf", PDF, &vec![b'x'; mib + 1])
        .await;
    let other_model = new_row(&env, normal, "big.pdf", PDF, &vec![b'x'; mib + 1]).await;

    assert_eq!(at_limit.size_bytes, mib as i64);
    assert_attachment_problem(&over, 400, "content_length", "FILE_TOO_LARGE");
    assert_eq!(
        other_model.size_bytes,
        (mib + 1) as i64,
        "a model with a larger max_file_size_mb is capped by the gear limit only"
    );
}

#[tokio::test]
async fn a_model_limit_of_zero_imposes_no_cap_and_the_gear_limit_applies() {
    let env = AttEnv::with(rag(|r| r.uploaded_file_max_size_kb = 2048)).await;
    let chat = env.chat("m-uncapped").await;
    let mib = 1_048_576;

    let big = new_row(&env, chat, "big.pdf", PDF, &vec![b'x'; 2 * mib]).await;
    let over = env
        .upload(chat, "bigger.pdf", PDF, &vec![b'x'; 2 * mib + 1])
        .await;

    assert_eq!(big.size_bytes, (2 * mib) as i64);
    assert_attachment_problem(&over, 400, "content_length", "FILE_TOO_LARGE");
}

#[tokio::test]
async fn the_size_of_an_image_is_lowered_by_the_model_limit_too() {
    let env = AttEnv::new().await;
    let small = env.chat("m-small").await;

    let reply = env
        .upload(small, "big.png", "image/png", &vec![0_u8; 1_048_577])
        .await;

    assert_attachment_problem(&reply, 400, "content_length", "FILE_TOO_LARGE");
}

#[tokio::test]
async fn an_empty_file_passes_validation() {
    let (env, chat) = setup().await;

    let row = new_row(&env, chat, "empty.txt", "text/plain", b"").await;

    assert_eq!(row.size_bytes, 0);
}

#[tokio::test]
async fn an_oversized_body_is_refused_without_waiting_for_the_rest_of_it() {
    let env = AttEnv::with(rag(|r| r.uploaded_file_max_size_kb = 1)).await;
    let chat = env.chat("m-oa").await;
    let mut held = held_body();
    // The file already exceeds the limit in the first chunk; the body never ends.
    held.send(&multipart_head("big.pdf", PDF, &[b'x'; 2048]));
    let body = held.take();

    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        env.upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            body,
        ),
    )
    .await
    .expect("the answer does not wait for the end of the body");

    assert_attachment_problem(&reply, 400, "content_length", "FILE_TOO_LARGE");
    assert_eq!(env.att_count().await, 0);
    assert_eq!(env.oagw.proxy_call_count(), 0);
    drop(held);
}

// ================================================================================================
// What is stored for an accepted file
// ================================================================================================

#[tokio::test]
async fn the_pending_row_records_the_upload() {
    let (env, chat) = setup().await;

    let row = new_row(&env, chat, "report.pdf", PDF, b"%PDF-1.7 content").await;

    assert_eq!(row.filename, "report.pdf");
    assert_eq!(row.content_type, PDF);
    assert_eq!(row.size_bytes, 16);
    assert_eq!(row.uploaded_by, USER);
    assert_eq!(row.tenant, USER_TENANT);
    assert_eq!(row.chat, chat);
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(row.doc_summary, None);
    assert_eq!(row.summary_model, None);
}

#[tokio::test]
async fn the_storage_backend_label_is_the_one_of_the_storage_provider() {
    let env = AttEnv::new().await;
    let azure = env.chat("m-az").await;
    let anthropic = env.chat("m-an").await;

    let on_azure = new_row(&env, azure, "a.pdf", PDF, b"%PDF").await;
    let on_anthropic = new_row(&env, anthropic, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(on_azure.storage_backend, "azure");
    assert_eq!(
        on_anthropic.storage_backend, "openai",
        "an Anthropic chat stores in its rag_provider"
    );
}

#[tokio::test]
async fn the_file_name_defaults_and_is_cut_to_255_characters_keeping_the_extension() {
    let (env, chat) = setup().await;
    let body = multipart_body(
        BOUNDARY,
        &[{
            let mut part = file_part("x", PDF, b"%PDF");
            part.filename = None;
            part
        }],
    );
    let _ = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;
    let (_, unnamed) = env.only_att().await;
    let long = format!("{}.pdf", "\u{e9}".repeat(300));
    let cut = new_row(&env, chat, &long, PDF, b"%PDF").await;

    assert_eq!(unnamed.filename, "upload");
    assert_eq!(cut.filename.chars().count(), 255);
    assert!(cut.filename.ends_with(".pdf"));
}

// ================================================================================================
// Check order after the body: size, provider_mismatch, then the insert with its limits
// ================================================================================================

#[tokio::test]
async fn the_size_check_comes_before_the_provider_mismatch_check() {
    let env = AttEnv::with(rag(|r| r.uploaded_file_max_size_kb = 1)).await;
    let chat = env.chat("m-oa").await;
    env.seed_store(chat, Some("vs_az"), "azure", env.now())
        .await;

    let reply = env.upload(chat, "big.pdf", PDF, &vec![b'x'; 4096]).await;

    assert_attachment_problem(&reply, 400, "content_length", "FILE_TOO_LARGE");
}

#[tokio::test]
async fn the_provider_mismatch_check_comes_before_the_chat_limits() {
    let env = AttEnv::with(rag(|r| r.max_documents_per_chat = 1)).await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;
    env.seed_store(chat, Some("vs_az"), "azure", env.now())
        .await;

    let reply = env.upload(chat, "second.pdf", PDF, b"%PDF").await;

    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert_eq!(reply.json["context"]["resource_name"], "provider_mismatch");
}

#[tokio::test]
async fn the_content_is_read_before_the_per_chat_limits_are_checked() {
    let env = AttEnv::with(rag(|r| r.max_documents_per_chat = 1)).await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;
    let mut held = held_body();
    held.send(&multipart_head("second.pdf", PDF, b"%PDF"));
    let body = held.take();
    let ctx = user_ctx();
    let content_type = multipart_content_type(BOUNDARY);
    let started = env.upload_raw(&ctx, chat, Some(&content_type), body);
    tokio::pin!(started);

    let early = tokio::time::timeout(Duration::from_millis(300), &mut started).await;
    assert!(
        early.is_err(),
        "the upload waits for the end of the file content"
    );
    held.send(&multipart_tail());
    let reply = started.await;

    limit_violation(&reply, "document_limit");
}

// ================================================================================================
// Per-chat limits
// ================================================================================================

fn limit_violation(reply: &Reply, reason: &str) {
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        reply.text
    );
    assert!(
        reply.category().contains("resource_exhausted"),
        "{}",
        reply.text
    );
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(violation["subject"], reason, "{}", reply.text);
    assert_eq!(violation["description"], reason, "{}", reply.text);
}

#[tokio::test]
async fn the_document_limit_is_429_document_limit_and_images_are_still_accepted() {
    let env = AttEnv::with(rag(|r| r.max_documents_per_chat = 2)).await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;
    env.seed(chat, 2, |row| row).await;

    let reply = env.upload(chat, "third.pdf", PDF, b"%PDF").await;
    let calls = env.oagw.proxy_call_count();
    let image = new_row(
        &env,
        chat,
        "a.png",
        "image/png",
        &crate::attachment_test_env::png_bytes(4, 4),
    )
    .await;

    limit_violation(&reply, "document_limit");
    assert_eq!(calls, 0, "no provider request");
    assert_eq!(
        env.att_count().await,
        3,
        "the rejected document left no row; the image was inserted"
    );
    assert_eq!(image.kind, "image");
}

#[tokio::test]
async fn a_failed_or_deleted_document_frees_its_slot() {
    let env = AttEnv::with(rag(|r| r.max_documents_per_chat = 2)).await;
    env.script_happy();
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| row).await;
    env.seed(chat, 2, |row| row.set("status", "failed")).await;
    let accepted = env.upload(chat, "third.pdf", PDF, b"%PDF").await;
    env.seed(chat, 3, |row| row.set("deleted_at", ts(5))).await;

    let blocked = env.upload(chat, "fourth.pdf", PDF, b"%PDF").await;

    assert_eq!(accepted.status, StatusCode::CREATED, "{}", accepted.text);
    limit_violation(&blocked, "document_limit");
}

#[tokio::test]
async fn the_storage_limit_is_429_storage_limit_and_counts_images() {
    let env = AttEnv::with(rag(|r| r.max_total_upload_mb_per_chat = 1)).await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("attachment_kind", "image")
            .set("for_file_search", false)
            .set("size_bytes", 600_i64 * 1024)
    })
    .await;

    let reply = env
        .upload(chat, "doc.pdf", PDF, &vec![b'x'; 600 * 1024])
        .await;
    let fits = new_row(&env, chat, "small.pdf", PDF, &vec![b'x'; 400 * 1024]).await;

    limit_violation(&reply, "storage_limit");
    assert_eq!(env.att_count().await, 2, "no row for the rejected one");
    assert_eq!(
        fits.size_bytes,
        400 * 1024,
        "exactly the remaining space is accepted"
    );
}

#[tokio::test]
async fn failed_and_deleted_rows_do_not_count_toward_the_storage_limit() {
    let env = AttEnv::with(rag(|r| r.max_total_upload_mb_per_chat = 1)).await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("status", "failed")
            .set("size_bytes", 900_i64 * 1024)
    })
    .await;
    env.seed(chat, 2, |row| {
        row.set("deleted_at", ts(5))
            .set("size_bytes", 900_i64 * 1024)
    })
    .await;

    let accepted = new_row(&env, chat, "doc.pdf", PDF, &vec![b'x'; 600 * 1024]).await;

    assert_eq!(accepted.size_bytes, 600 * 1024);
}

#[tokio::test]
async fn the_limits_are_per_chat() {
    let env = AttEnv::with(rag(|r| r.max_documents_per_chat = 1)).await;
    let full = env.chat("m-oa").await;
    let empty = env.chat("m-oa").await;
    env.seed(full, 1, |row| row).await;

    let accepted = new_row(&env, empty, "a.pdf", PDF, b"%PDF").await;

    assert_eq!(accepted.chat, empty);
}

// ================================================================================================
// The concurrency permit
// ================================================================================================

fn one_permit() -> AttOptions {
    rag(|r| r.max_concurrent_uploads = 1)
}

/// Starts an upload whose body stays open (the request holds its permit) and
/// returns the pending request and the feeder of its body.
async fn hold_a_permit(
    env: &std::sync::Arc<AttEnv>,
    chat: Uuid,
) -> (
    tokio::task::JoinHandle<Reply>,
    crate::attachment_test_env::HeldBody,
) {
    let mut held = held_body();
    held.send(&multipart_head("a.pdf", PDF, b"%PDF-1.7"));
    let body = held.take();
    let task_env = std::sync::Arc::clone(env);
    let task = tokio::spawn(async move {
        task_env
            .upload_raw(
                &user_ctx(),
                chat,
                Some(&multipart_content_type(BOUNDARY)),
                body,
            )
            .await
    });
    for _ in 0..500 {
        if held.idle.load(Ordering::SeqCst) > 0 {
            return (task, held);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the first upload never started waiting for its body");
}

#[tokio::test]
async fn an_upload_over_the_concurrency_limit_is_503_retry_after_5_without_reading_its_body() {
    let env = std::sync::Arc::new(AttEnv::with(one_permit()).await);
    let chat = env.chat("m-oa").await;
    let (first, held) = hold_a_permit(&env, chat).await;
    let (body, polls) = counting_body(vec![multipart_body(
        BOUNDARY,
        &[file_part("b.pdf", PDF, b"x")],
    )]);

    let reply = env
        .upload_raw(
            &user_ctx(),
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            body,
        )
        .await;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
    assert_eq!(reply.json["context"]["retry_after_seconds"], 5);
    assert_eq!(polls.load(Ordering::SeqCst), 0, "the body is not read");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.att_count().await, 0, "no row for the rejected request");
    drop(held);
    first.abort();
}

#[tokio::test]
async fn the_permit_is_released_when_the_upload_ends() {
    let env = std::sync::Arc::new(AttEnv::with(one_permit()).await);
    let chat = env.chat("m-oa").await;
    let (first, held) = hold_a_permit(&env, chat).await;
    drop(held);
    let ended = first.await.unwrap();

    let next = env.upload(chat, "b.zip", "application/zip", b"x").await;

    assert_eq!(
        ended.status,
        StatusCode::BAD_REQUEST,
        "the first body ended mid-part: {}",
        ended.text
    );
    assert_attachment_problem(&next, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
}

#[tokio::test]
async fn a_client_that_disconnects_releases_its_permit() {
    let env = std::sync::Arc::new(AttEnv::with(one_permit()).await);
    let chat = env.chat("m-oa").await;
    let (first, _held) = hold_a_permit(&env, chat).await;

    first.abort();
    drop(first.await);
    let next = env.upload(chat, "b.zip", "application/zip", b"x").await;

    assert_attachment_problem(&next, 400, "file", "UNSUPPORTED_CONTENT_TYPE");
}

#[tokio::test]
async fn the_model_and_chat_checks_come_before_the_permit() {
    let env = std::sync::Arc::new(AttEnv::with(one_permit()).await);
    let chat = env.chat("m-oa").await;
    let retired = env.chat("retired-model").await;
    let (first, held) = hold_a_permit(&env, chat).await;

    let invalid_model = env.upload(retired, "b.pdf", PDF, b"x").await;
    let unknown_chat = env.upload(Uuid::new_v4(), "b.pdf", PDF, b"x").await;

    invalid_model.assert_invalid("model", "INVALID_MODEL");
    unknown_chat.assert_not_found(CHAT_TYPE);
    drop(held);
    first.abort();
}

#[tokio::test]
async fn the_limit_is_per_process_not_per_chat() {
    let env = std::sync::Arc::new(AttEnv::with(one_permit()).await);
    let chat = env.chat("m-oa").await;
    let other = env.chat("m-oa").await;
    let (first, held) = hold_a_permit(&env, chat).await;

    let reply = env.upload(other, "b.pdf", PDF, b"x").await;

    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    drop(held);
    first.abort();
    let _ = json!({});
}
