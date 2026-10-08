//! Shared helpers of the streaming-core tests (tests only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    dead_code
)]

use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures::StreamExt;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::dto::MiniChatSseEvent;
use crate::domain::service::stream::{SendInput, StreamStart};
use crate::domain::service::test_support::{TestEnv, TestOptions};
use crate::infra::db::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, thread_summary,
};

pub async fn env_with(f: impl FnOnce(&mut TestOptions)) -> TestEnv {
    let mut o = TestOptions::default();
    f(&mut o);
    TestEnv::new(o).await
}

pub async fn create_chat(env: &TestEnv, user: Uuid, tenant: Uuid, model: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let am = chat::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        user_id: Set(user),
        model: Set(model.to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .unwrap();
    id
}

/// Attachment options.
pub struct Att<'a> {
    pub kind: &'a str,
    pub status: &'a str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub provider_file_id: Option<&'a str>,
    pub filename: &'a str,
    pub uploaded_by: Uuid,
}

impl Default for Att<'_> {
    fn default() -> Self {
        Self {
            kind: "document",
            status: "ready",
            for_file_search: true,
            for_code_interpreter: false,
            provider_file_id: Some("file-doc0000000000001"),
            filename: "report.pdf",
            uploaded_by: crate::domain::service::test_support::USER_A1,
        }
    }
}

pub async fn add_attachment(env: &TestEnv, chat_id: Uuid, tenant: Uuid, a: Att<'_>) -> Uuid {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let am = attachment::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(a.uploaded_by),
        filename: Set(a.filename.to_owned()),
        content_type: Set(if a.kind == "image" {
            "image/png"
        } else {
            "application/pdf"
        }
        .to_owned()),
        size_bytes: Set(10),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(a.provider_file_id.map(str::to_owned)),
        status: Set(a.status.to_owned()),
        error_code: Set(None),
        attachment_kind: Set(a.kind.to_owned()),
        for_file_search: Set(a.for_file_search),
        for_code_interpreter: Set(a.for_code_interpreter),
        doc_summary: Set(None),
        img_thumbnail: Set(None),
        img_thumbnail_width: Set(None),
        img_thumbnail_height: Set(None),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .unwrap();
    id
}

pub async fn add_vector_store(env: &TestEnv, chat_id: Uuid, tenant: Uuid, vs: &str) {
    let am = chat_vector_store::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        vector_store_id: Set(Some(vs.to_owned())),
        provider: Set("openai".to_owned()),
        file_count: Set(1),
        created_at: Set(OffsetDateTime::now_utc()),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .unwrap();
}

#[must_use]
pub fn input(content: &str) -> SendInput {
    SendInput {
        content: content.to_owned(),
        ..SendInput::default()
    }
}

/// Collects all events of a setup (live streams until the terminal event).
pub async fn collect(start: StreamStart) -> Vec<MiniChatSseEvent> {
    match start {
        StreamStart::Replay(v) => v,
        StreamStart::Live(l) => {
            tokio::time::timeout(Duration::from_secs(20), l.into_events().collect::<Vec<_>>())
                .await
                .expect("stream did not finish")
        }
    }
}

#[must_use]
pub fn names(evs: &[MiniChatSseEvent]) -> Vec<&'static str> {
    evs.iter().map(MiniChatSseEvent::name).collect()
}

pub fn live(start: StreamStart) -> crate::domain::service::stream::LiveStream {
    match start {
        StreamStart::Live(l) => l,
        StreamStart::Replay(_) => panic!("expected a live stream"),
    }
}

pub async fn turn(env: &TestEnv, chat_id: Uuid, request_id: Uuid) -> chat_turn::Model {
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("turn")
}

pub async fn turns(env: &TestEnv, chat_id: Uuid) -> Vec<chat_turn::Model> {
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(chat_turn::Column::StartedAt, Order::Asc)
        .all(&conn)
        .await
        .unwrap()
}

/// All messages of the chat (including deleted), chronological.
pub async fn messages(env: &TestEnv, chat_id: Uuid) -> Vec<message::Model> {
    let conn = env.deps.db.conn().unwrap();
    message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(&conn)
        .await
        .unwrap()
}

pub async fn summary_row(env: &TestEnv, chat_id: Uuid) -> Option<thread_summary::Model> {
    let conn = env.deps.db.conn().unwrap();
    thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
}

/// Polls until the turn left `running` (5 s max).
pub async fn wait_terminal(env: &TestEnv, chat_id: Uuid, request_id: Uuid) -> chat_turn::Model {
    for _ in 0..100 {
        let t = turn(env, chat_id, request_id).await;
        if t.state != "running" {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("turn stayed running");
}

/// Event payload by name.
#[must_use]
pub fn data(evs: &[MiniChatSseEvent], name: &str) -> serde_json::Value {
    evs.iter()
        .find(|e| e.name() == name)
        .map(MiniChatSseEvent::data_json)
        .unwrap_or(serde_json::Value::Null)
}

/// Request id from `stream_started`.
#[must_use]
pub fn started_request_id(evs: &[MiniChatSseEvent]) -> Uuid {
    Uuid::parse_str(data(evs, "stream_started")["request_id"].as_str().unwrap()).unwrap()
}

// ── HTTP ───────────────────────────────────────────────────────────────────

pub fn router(env: &TestEnv) -> Router {
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    crate::api::rest::routes::register_routes(
        Router::new(),
        &openapi,
        env.services.clone(),
        "/mini-chat",
    )
}

pub fn request(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    ctx: SecurityContext,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap())))
        .unwrap();
    req.extensions_mut().insert(ctx);
    req
}

pub async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    tokio::time::timeout(
        Duration::from_secs(20),
        to_bytes(resp.into_body(), 16 * 1024 * 1024),
    )
    .await
    .expect("body timeout")
    .unwrap()
    .to_vec()
}

/// Parses `event: X\ndata: {...}\n\n` blocks (comments ignored).
#[must_use]
pub fn parse_sse(bytes: &[u8]) -> Vec<(String, serde_json::Value)> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                name = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim_start());
            }
        }
        if let Some(n) = name {
            out.push((
                n,
                serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
            ));
        }
    }
    out
}
