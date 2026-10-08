//! Test helpers shared by the attachment, cleanup and reaper tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::io::Cursor;

use axum::body::Body;
use http::{HeaderMap, Request, StatusCode};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::infra::db::entities::{attachment, chat_vector_store, message, message_attachment};
use crate::testing::{RecordedRequest, TestApp};

pub const BOUNDARY: &str = "XyZboundary123";

/// One multipart part: `(name, filename, content type, bytes)`.
pub type Part<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

#[must_use]
pub fn multipart_body(parts: &[Part<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, filename, ct, data) in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disp = format!("Content-Disposition: form-data; name=\"{name}\"");
        if let Some(f) = filename {
            disp = format!("{disp}; filename=\"{f}\"");
        }
        out.extend_from_slice(disp.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = ct {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// Raw upload with an explicit `Content-Type` header and body.
pub async fn upload_raw(
    t: &TestApp,
    ctx: &SecurityContext,
    chat_id: Uuid,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat_id}/attachments"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
    t.call_raw(ctx, req).await
}

/// Uploads one `file` part.
pub async fn upload(
    t: &TestApp,
    ctx: &SecurityContext,
    chat_id: Uuid,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let body = multipart_body(&[("file", Some(filename), Some(content_type), data)]);
    upload_raw(t, ctx, chat_id, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await
}

/// Uploads a small PDF-like document and asserts 201.
pub async fn upload_doc(t: &TestApp, ctx: &SecurityContext, chat_id: Uuid) -> Uuid {
    let (st, _, json) = upload(t, ctx, chat_id, "report.pdf", "application/pdf", b"%PDF-1.4 hello").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    json["id"].as_str().unwrap().parse().unwrap()
}

/// In-memory PNG.
#[must_use]
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([u8::try_from(x % 256).unwrap(), u8::try_from(y % 256).unwrap(), 128]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// All attachment rows of a chat (any state), oldest first.
pub async fn rows(t: &TestApp, chat_id: Uuid) -> Vec<attachment::Model> {
    let conn = t.app.db.conn().unwrap();
    attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .order_by_asc(attachment::Column::CreatedAt)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

pub async fn row(t: &TestApp, id: Uuid) -> attachment::Model {
    let conn = t.app.db.conn().unwrap();
    attachment::Entity::find()
        .filter(attachment::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment row")
}

pub async fn vector_store_rows(t: &TestApp, chat_id: Uuid) -> Vec<chat_vector_store::Model> {
    let conn = t.app.db.conn().unwrap();
    chat_vector_store::Entity::find()
        .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

/// Recorded provider requests matching `method` whose URI contains `needle`.
#[must_use]
pub fn requests(t: &TestApp, method: &str, needle: &str) -> Vec<RecordedRequest> {
    t.provider.recorded().into_iter().filter(|r| r.method == method && r.uri.contains(needle)).collect()
}

/// Provider `DELETE /files/{id}` requests.
#[must_use]
pub fn file_deletes(t: &TestApp) -> usize {
    requests(t, "DELETE", "/files/").len()
}

/// Row template for direct inserts.
#[must_use]
pub fn row_template(tenant: Uuid, chat_id: Uuid, uploader: Uuid, status: &str, updated_at: OffsetDateTime) -> attachment::Model {
    attachment::Model {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        chat_id,
        uploaded_by_user_id: uploader,
        filename: "direct.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size_bytes: 10,
        storage_backend: "openai".to_owned(),
        provider_file_id: None,
        status: status.to_owned(),
        error_code: None,
        attachment_kind: "document".to_owned(),
        for_file_search: true,
        for_code_interpreter: false,
        doc_summary: None,
        img_thumbnail: None,
        img_thumbnail_width: None,
        img_thumbnail_height: None,
        summary_model: None,
        summary_updated_at: None,
        cleanup_status: None,
        cleanup_attempts: 0,
        last_cleanup_error: None,
        cleanup_updated_at: None,
        created_at: updated_at,
        updated_at,
        deleted_at: None,
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        secondary_provider_kind: None,
    }
}

/// Inserts an attachment row as given.
pub async fn insert_row(t: &TestApp, m: attachment::Model) -> Uuid {
    let id = m.id;
    let tenant = m.tenant_id;
    let am: attachment::ActiveModel = m.into();
    let conn = t.app.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::for_tenant(tenant), &conn).await.unwrap();
    id
}

/// Inserts a user message referencing `attachment_id` (optionally soft-deleted).
pub async fn insert_message_link(t: &TestApp, tenant: Uuid, chat_id: Uuid, attachment_id: Uuid, deleted: bool) -> Uuid {
    let now = clock::now();
    let msg_id = Uuid::new_v4();
    let conn = t.app.db.conn().unwrap();
    let scope = AccessScope::for_tenant(tenant);
    let msg = message::ActiveModel {
        id: Set(msg_id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(Some(Uuid::new_v4())),
        role: Set("user".to_owned()),
        content: Set("see attached".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(3),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(if deleted { Some(now) } else { None }),
    };
    secure_insert::<message::Entity>(msg, &scope, &conn).await.unwrap();
    let link = message_attachment::ActiveModel {
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        message_id: Set(msg_id),
        attachment_id: Set(attachment_id),
        created_at: Set(now),
    };
    secure_insert::<message_attachment::Entity>(link, &scope, &conn).await.unwrap();
    msg_id
}

/// Stops the outbox pipeline so handlers can be driven directly.
pub async fn stop_outbox(t: &mut TestApp) {
    if let Some(h) = t.outbox.take() {
        h.stop().await;
    }
}

/// Outbox message for a direct handler call.
#[must_use]
pub fn outbox_message<T: serde::Serialize>(payload: &T, payload_type: &str, attempts: i16) -> toolkit_db::outbox::OutboxMessage {
    toolkit_db::outbox::OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: serde_json::to_vec(payload).unwrap(),
        payload_type: payload_type.to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}
