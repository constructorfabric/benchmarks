//! Fixtures of the attachment tests: multipart request bodies, scripted provider storage
//! (files, vector stores), small images and database reads (all through `TestApp`).

use std::time::Duration;

use axum::body::Body;
use http::{Method, Request};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, SecureDeleteExt as _, SecureEntityExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::app::{TestApp, TestResponse};
use super::gateway::Responder;
use super::stream::CHATS;
use crate::domain::attachment::IndexingTimings;
use crate::infra::db::entity::{attachments, chat_vector_stores};
use crate::infra::db::ts::{db_now, normalize};

/// Path of the provider file uploads (`POST`) and deletes (`DELETE /v1/files/{id}`).
pub const FILES_PATH: &str = "/v1/files";
/// Path of the vector store creation (`POST`).
pub const VECTOR_STORES_PATH: &str = "/v1/vector_stores";
/// Vector store id answered by [`script_vector_store`].
pub const VS_ID: &str = "vs_test1";
/// Boundary of [`multipart_body`].
pub const BOUNDARY: &str = "mini-chat-test-boundary";
pub const PDF: &str = "application/pdf";
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// Queue of the attachment cleanup events.
pub const CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";

/// Indexing timings small enough for tests: 200 ms request deadline, 10 → 20 ms request polls,
/// background rounds of 100 ms (polls 10 → 20 ms) for at most `background_total`.
pub fn fast_timings(background_total: Duration) -> IndexingTimings {
    IndexingTimings {
        request_deadline: Duration::from_millis(200),
        request_poll_initial: Duration::from_millis(10),
        request_poll_max: Duration::from_millis(20),
        background_total,
        background_round: Duration::from_millis(100),
        background_poll_max: Duration::from_millis(20),
    }
}

/// `{CHATS}/{chat}/attachments`.
pub fn attachments_uri(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/attachments")
}

/// `{CHATS}/{chat}/attachments/{id}`.
pub fn attachment_uri(chat: Uuid, id: impl std::fmt::Display) -> String {
    format!("{CHATS}/{chat}/attachments/{id}")
}

/// One part of a multipart body.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl Part {
    /// A `file` part.
    pub fn file(filename: Option<&str>, content_type: Option<&str>, data: &[u8]) -> Self {
        Self {
            name: "file".to_owned(),
            filename: filename.map(str::to_owned),
            content_type: content_type.map(str::to_owned),
            data: data.to_vec(),
        }
    }

    /// A plain text field.
    pub fn text(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            filename: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }
}

/// `multipart/form-data` body of `parts` with [`BOUNDARY`].
pub fn multipart_body(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = &part.filename {
            disposition = format!("{disposition}; filename=\"{filename}\"");
        }
        out.extend_from_slice(disposition.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = &part.content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&part.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// `POST {attachments_uri(chat)}` as `who` with the raw `content_type` header and `body`.
pub async fn post_upload(
    app: &TestApp,
    who: &SecurityContext,
    chat: Uuid,
    content_type: &str,
    body: Vec<u8>,
) -> TestResponse {
    let mut req = Request::builder()
        .method("POST")
        .uri(attachments_uri(chat))
        .header(http::header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("request");
    req.extensions_mut().insert(who.clone());
    app.call_buffered(req).await
}

/// Uploads one `file` part (`filename`, `content_type`, `data`).
pub async fn upload(
    app: &TestApp,
    who: &SecurityContext,
    chat: Uuid,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> TestResponse {
    let body = multipart_body(&[Part::file(Some(filename), Some(content_type), data)]);
    post_upload(
        app,
        who,
        chat,
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        body,
    )
    .await
}

/// The `id` of an attachment response.
pub fn id_of(res: &TestResponse) -> Uuid {
    res.json["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no attachment id in {}", res.json))
        .parse()
        .expect("uuid")
}

/// The next file uploads answer with `ids`, one each, in order.
pub fn script_files(app: &TestApp, ids: &[&str]) {
    let responders = ids
        .iter()
        .map(|id| Responder::json(200, json!({ "id": id, "object": "file" })))
        .collect();
    app.gateway
        .on_sequence(Method::POST, FILES_PATH, responders);
}

/// Vector store creation answers `vs_id` (after `create_delay`); adding a file to it answers
/// `add_file`.
pub fn script_vector_store(app: &TestApp, vs_id: &str, create_delay: Duration, add_file: Value) {
    let created = Responder::json(200, json!({ "id": vs_id, "object": "vector_store" }));
    let created = if create_delay.is_zero() {
        created
    } else {
        Responder::Delayed(create_delay, Box::new(created))
    };
    app.gateway.on(Method::POST, VECTOR_STORES_PATH, created);
    app.gateway.on(
        Method::POST,
        &format!("/vector_stores/{vs_id}/files"),
        Responder::json(200, add_file),
    );
}

/// Every status read of a file in `vs_id` answers `status` (replaces earlier scripts).
pub fn script_index_status(app: &TestApp, vs_id: &str, status: Value) {
    app.gateway.on(
        Method::GET,
        &format!("/vector_stores/{vs_id}/files/"),
        Responder::json(200, status),
    );
}

/// Provider file deletes answer 200.
pub fn script_file_deletes(app: &TestApp) {
    app.gateway.on(
        Method::DELETE,
        FILES_PATH,
        Responder::json(200, json!({ "deleted": true })),
    );
}

/// Vector store creations recorded so far (`POST …/v1/vector_stores` exactly).
pub fn vector_store_creates(app: &TestApp) -> usize {
    app.gateway
        .requests_to(&Method::POST, VECTOR_STORES_PATH)
        .iter()
        .filter(|r| r.uri.ends_with(VECTOR_STORES_PATH))
        .count()
}

/// A `width × height` RGB PNG.
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        let low_byte = |v: u32| v.to_le_bytes()[0];
        image::Rgb([low_byte(x), low_byte(y), 128])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

/// Every attachment row of `chat` (deleted ones included), oldest first.
pub async fn attachment_rows(app: &TestApp, chat: Uuid) -> Vec<attachments::Model> {
    let conn = app.db.conn().expect("conn");
    attachments::Entity::find()
        .filter(attachments::Column::ChatId.eq(chat))
        .order_by_asc(attachments::Column::CreatedAt)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read attachments")
}

/// The attachment row `id`.
pub async fn attachment_row(app: &TestApp, chat: Uuid, id: Uuid) -> attachments::Model {
    attachment_rows(app, chat)
        .await
        .into_iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("attachment {id} not found"))
}

/// The `chat_vector_stores` rows of `chat`.
pub async fn vector_store_rows(app: &TestApp, chat: Uuid) -> Vec<chat_vector_stores::Model> {
    let conn = app.db.conn().expect("conn");
    chat_vector_stores::Entity::find()
        .filter(chat_vector_stores::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read vector stores")
}

/// Inserts the `chat_vector_stores` row of `chat` with `vector_store_id` and `provider`.
pub async fn seed_vector_store_row(
    app: &TestApp,
    tenant: Uuid,
    chat: Uuid,
    vector_store_id: Option<&str>,
    provider: &str,
) {
    seed_vector_store_row_at(app, tenant, chat, vector_store_id, provider, db_now()).await;
}

/// Like [`seed_vector_store_row`] with an explicit `created_at`.
pub async fn seed_vector_store_row_at(
    app: &TestApp,
    tenant: Uuid,
    chat: Uuid,
    vector_store_id: Option<&str>,
    provider: &str,
    created_at: OffsetDateTime,
) {
    let row = chat_vector_stores::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat),
        vector_store_id: Set(vector_store_id.map(str::to_owned)),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(normalize(created_at)),
    };
    let conn = app.db.conn().expect("conn");
    secure_insert::<chat_vector_stores::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed vector store row");
}

/// Removes every `chat_vector_stores` row of `chat`.
pub async fn delete_vector_store_rows(app: &TestApp, chat: Uuid) {
    let conn = app.db.conn().expect("conn");
    chat_vector_stores::Entity::delete_many()
        .filter(chat_vector_stores::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("delete vector store rows");
}

/// Gives attachment `id` an uploaded secondary (Anthropic) copy `file_id`.
pub async fn set_secondary_file(app: &TestApp, id: Uuid, file_id: &str) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    attachments::Entity::update_many()
        .col_expr(
            attachments::Column::SecondaryFileId,
            Expr::value(Some(file_id.to_owned())),
        )
        .col_expr(
            attachments::Column::SecondaryStatus,
            Expr::value("uploaded"),
        )
        .col_expr(
            attachments::Column::SecondaryProviderKind,
            Expr::value(Some("anthropic")),
        )
        .filter(attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("set secondary file");
}

/// Overwrites the cleanup columns of attachment `id`, as a concurrent cleanup delivery would.
pub async fn set_cleanup_state(app: &TestApp, id: Uuid, status: &str, attempts: i32) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupStatus,
            Expr::value(Some(status.to_owned())),
        )
        .col_expr(attachments::Column::CleanupAttempts, Expr::value(attempts))
        .filter(attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("set cleanup state");
}
