//! Fixtures of the streaming tests: chats, scripted `OpenAI` Responses streams, seeded rows and
//! database reads (all through `TestApp`).

use http::Method;
use sea_orm::ActiveValue::NotSet;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::app::{SseFrame, TestApp};
use super::gateway::{BodyMatcher, Responder, SseScript};
use crate::domain::quota::periods::period_starts;
use crate::domain::quota::{Bucket, Period};
use crate::infra::db::entity::{
    attachments, chat_turns, chat_vector_stores, chats, message_attachments, message_reactions,
    messages, quota_usage, thread_summaries,
};
use crate::infra::db::repo::quota_usage::{self as quota_repo, BucketDelta, BucketKey};
use crate::infra::db::ts::db_now;

pub const CHATS: &str = "/mini-chat/v1/chats";
/// Path of the chat calls of the test provider (`openai_responses`).
pub const RESPONSES_PATH: &str = "/v1/responses";
pub const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
pub const AUDIT_QUEUE: &str = "mini-chat.audit";
pub const SUMMARY_QUEUE: &str = "mini-chat.thread_summary";

/// `POST` target of a send.
pub fn stream_uri(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/messages:stream")
}

/// Creates a chat as `who` (the default model when `model` is `None`).
pub async fn create_chat(app: &TestApp, who: &SecurityContext, model: Option<&str>) -> Uuid {
    let body = model.map_or_else(|| json!({}), |m| json!({ "model": m }));
    let res = app.call("POST", CHATS, who, Some(body)).await;
    assert_eq!(res.status, 201, "{}", res.json);
    res.json["id"]
        .as_str()
        .expect("chat id")
        .parse()
        .expect("uuid")
}

/// `response.output_text.delta` with `text`.
pub fn text_delta(text: &str) -> SseScript {
    SseScript::event(
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "delta": text}),
    )
}

/// `response.completed` of response `resp_1` with the given usage.
pub fn completed(input_tokens: i64, output_tokens: i64) -> SseScript {
    SseScript::event(
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_1",
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        }}),
    )
}

/// `response.failed` with `message`.
pub fn failed(message: &str) -> SseScript {
    SseScript::event(
        "response.failed",
        json!({"type": "response.failed", "response": {"error": {"message": message}}}),
    )
}

/// `response.failed` carrying `response.usage`.
pub fn failed_with_usage(message: &str, input_tokens: i64, output_tokens: i64) -> SseScript {
    SseScript::event(
        "response.failed",
        json!({"type": "response.failed", "response": {
            "error": {"message": message},
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        }}),
    )
}

/// `response.incomplete` with `reason` and the given usage.
pub fn incomplete(reason: &str, input_tokens: i64, output_tokens: i64) -> SseScript {
    SseScript::event(
        "response.incomplete",
        json!({"type": "response.incomplete", "response": {
            "id": "resp_1",
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
            "incomplete_details": {"reason": reason},
        }}),
    )
}

/// A data-only Responses event of type `kind` (e.g. `response.web_search_call.searching`).
pub fn provider_event(kind: &str) -> SseScript {
    SseScript::event(kind, json!({ "type": kind }))
}

/// `response.output_text.annotation.added` with `annotation` (index `n` of part 0/0).
pub fn annotation(n: u64, annotation: &Value) -> SseScript {
    SseScript::event(
        "response.output_text.annotation.added",
        json!({"type": "response.output_text.annotation.added",
               "output_index": 0, "content_index": 0, "annotation_index": n,
               "annotation": annotation}),
    )
}

/// `response.output_item.done` with a `function_call` item.
pub fn function_call(name: &str) -> SseScript {
    SseScript::event(
        "response.output_item.done",
        json!({"type": "response.output_item.done", "item": {
            "type": "function_call", "call_id": "call_1", "name": name, "arguments": "{}",
        }}),
    )
}

/// `response.output_item.done` with a `function_call` item `call_id` / `name` / `arguments`.
pub fn function_call_with(call_id: &str, name: &str, arguments: &Value) -> SseScript {
    SseScript::event(
        "response.output_item.done",
        json!({"type": "response.output_item.done", "item": {
            "type": "function_call", "call_id": call_id, "name": name,
            "arguments": arguments.to_string(),
        }}),
    )
}

/// Deltas of `texts`, then `completed(input_tokens, output_tokens)`.
pub fn answer(texts: &[&str], input_tokens: i64, output_tokens: i64) -> Responder {
    let mut script: Vec<SseScript> = texts.iter().map(|t| text_delta(t)).collect();
    script.push(completed(input_tokens, output_tokens));
    Responder::Sse(script)
}

/// Path of the chat calls of the Anthropic test provider (`app::anthropic_provider`).
pub const ANTHROPIC_MESSAGES_PATH: &str = "/anthropic.test/v1/messages";

/// An Anthropic Messages SSE event `name` (the `type` field of `data` is set to `name`).
pub fn anthropic_event(name: &str, mut data: Value) -> SseScript {
    data["type"] = json!(name);
    SseScript::event(name, data)
}

/// An Anthropic Messages stream: `message_start` (10 input tokens), the text deltas of one text
/// block, `message_delta` (`end_turn`, `output_tokens`) and `message_stop`.
pub fn anthropic_answer(texts: &[&str], output_tokens: i64) -> Responder {
    let ev = anthropic_event;
    let mut script = vec![
        ev(
            "message_start",
            json!({"message": {"id": "msg_1", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        ),
        ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
    ];
    script.extend(texts.iter().map(|t| {
        ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": t}}),
        )
    }));
    script.push(ev("content_block_stop", json!({"index": 0})));
    script.push(ev(
        "message_delta",
        json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": output_tokens}}),
    ));
    script.push(ev("message_stop", json!({})));
    Responder::Sse(script)
}

/// Every chat call answers with `responder`.
pub fn script_provider(app: &TestApp, responder: Responder) {
    app.gateway.on(Method::POST, RESPONSES_PATH, responder);
}

/// Chat calls answer with `responders`, one per call.
pub fn script_provider_sequence(app: &TestApp, responders: Vec<Responder>) {
    app.gateway
        .on_sequence(Method::POST, RESPONSES_PATH, responders);
}

/// Recorded chat calls (their JSON bodies).
pub fn provider_calls(app: &TestApp) -> Vec<Value> {
    app.gateway
        .requests_to(&Method::POST, RESPONSES_PATH)
        .into_iter()
        .map(|r| r.json.expect("JSON provider request"))
        .collect()
}

/// Whether a provider request body is a thread summary call (`metadata.request_type`).
pub fn is_summary_call(body: &Value) -> bool {
    body["metadata"]["request_type"] == "summary"
}

/// Thread summary calls answer with `responder` (chat calls keep their own rules; install the
/// chat script first, this rule wins for summary calls).
pub fn script_summary(app: &TestApp, responder: Responder) {
    let matcher: BodyMatcher = std::sync::Arc::new(is_summary_call);
    app.gateway
        .on_matching(Method::POST, RESPONSES_PATH, matcher, responder);
}

/// A non-streaming Responses answer with `text` and the given output / reasoning tokens.
pub fn summary_response(text: &str, output_tokens: i64, reasoning_tokens: i64) -> Responder {
    Responder::json(
        200,
        json!({
            "id": "resp_summary",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 100, "output_tokens": output_tokens,
                      "output_tokens_details": {"reasoning_tokens": reasoning_tokens}},
        }),
    )
}

/// Recorded thread summary calls (their JSON bodies).
pub fn summary_calls(app: &TestApp) -> Vec<Value> {
    provider_calls(app)
        .into_iter()
        .filter(is_summary_call)
        .collect()
}

/// Recorded chat calls (their JSON bodies), without the thread summary calls.
pub fn chat_calls(app: &TestApp) -> Vec<Value> {
    provider_calls(app)
        .into_iter()
        .filter(|b| !is_summary_call(b))
        .collect()
}

/// Soft-deletes message `id` (`deleted_at = now`), as a retry, edit or delete would.
pub async fn soft_delete_message(app: &TestApp, id: Uuid) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(db_now())))
        .filter(messages::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("soft-delete message");
}

/// The first frame named `event`.
pub fn frame<'a>(frames: &'a [SseFrame], event: &str) -> &'a SseFrame {
    frames
        .iter()
        .find(|f| f.event == event)
        .unwrap_or_else(|| panic!("no `{event}` frame in {frames:?}"))
}

/// Event names in order.
pub fn event_names(frames: &[SseFrame]) -> Vec<&str> {
    frames.iter().map(|f| f.event.as_str()).collect()
}

/// An attachment row to seed.
#[derive(Debug, Clone)]
pub struct SeedAttachment {
    pub tenant: Uuid,
    pub chat: Uuid,
    pub uploader: Uuid,
    /// `document` or `image`.
    pub kind: &'static str,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: &'static str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Preview of an image: WebP bytes with their width and height.
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
}

impl SeedAttachment {
    /// A ready image of `chat` uploaded by `uploader`.
    pub fn image(tenant: Uuid, chat: Uuid, uploader: Uuid) -> Self {
        Self {
            tenant,
            chat,
            uploader,
            kind: "image",
            status: "ready",
            for_file_search: false,
            for_code_interpreter: false,
            thumbnail: None,
        }
    }

    /// A ready document of `chat` indexed for file search.
    pub fn document(tenant: Uuid, chat: Uuid, uploader: Uuid) -> Self {
        Self {
            kind: "document",
            for_file_search: true,
            ..Self::image(tenant, chat, uploader)
        }
    }
}

/// Inserts the attachment (provider file id `file-<simple uuid>`) and returns its id.
pub async fn seed_attachment(app: &TestApp, a: SeedAttachment) -> Uuid {
    let id = Uuid::new_v4();
    let now = db_now();
    let (filename, content_type) = if a.kind == "image" {
        ("photo.png", "image/png")
    } else {
        ("report.pdf", "application/pdf")
    };
    let am = attachments::ActiveModel {
        id: Set(id),
        tenant_id: Set(a.tenant),
        chat_id: Set(a.chat),
        uploaded_by_user_id: Set(a.uploader),
        filename: Set(filename.into()),
        content_type: Set(content_type.into()),
        size_bytes: Set(10),
        storage_backend: Set("openai".into()),
        provider_file_id: Set(Some(format!("file-{}", id.simple()))),
        status: Set(a.status.into()),
        error_code: NotSet,
        attachment_kind: Set(a.kind.into()),
        for_file_search: Set(a.for_file_search),
        for_code_interpreter: Set(a.for_code_interpreter),
        doc_summary: NotSet,
        img_thumbnail: a
            .thumbnail
            .as_ref()
            .map_or(NotSet, |t| Set(Some(t.0.clone()))),
        img_thumbnail_width: a.thumbnail.as_ref().map_or(NotSet, |t| Set(Some(t.1))),
        img_thumbnail_height: a.thumbnail.as_ref().map_or(NotSet, |t| Set(Some(t.2))),
        summary_model: NotSet,
        summary_updated_at: NotSet,
        cleanup_status: NotSet,
        cleanup_attempts: NotSet,
        last_cleanup_error: NotSet,
        cleanup_updated_at: NotSet,
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: NotSet,
        secondary_file_id: NotSet,
        secondary_status: NotSet,
        secondary_provider_kind: NotSet,
    };
    let conn = app.db.conn().expect("conn");
    secure_insert::<attachments::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed attachment");
    id
}

/// The chat row (also when soft-deleted).
pub async fn chat_row(app: &TestApp, chat: Uuid) -> chats::Model {
    let conn = app.db.conn().expect("conn");
    chats::Entity::find()
        .filter(chats::Column::Id.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .expect("read chat")
        .expect("chat row")
}

/// Every turn of `chat` (also soft-deleted ones), oldest first.
pub async fn turns_of(app: &TestApp, chat: Uuid) -> Vec<chat_turns::Model> {
    let conn = app.db.conn().expect("conn");
    chat_turns::Entity::find()
        .filter(chat_turns::Column::ChatId.eq(chat))
        .order_by_asc(chat_turns::Column::StartedAt)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read turns")
}

/// The turn of `(chat, request_id)`.
pub async fn turn_of(app: &TestApp, chat: Uuid, request_id: Uuid) -> chat_turns::Model {
    turns_of(app, chat)
        .await
        .into_iter()
        .find(|t| t.request_id == request_id)
        .unwrap_or_else(|| panic!("no turn for request {request_id}"))
}

/// Every message of `chat` (also soft-deleted ones), in `(created_at, id)` order.
pub async fn messages_of(app: &TestApp, chat: Uuid) -> Vec<messages::Model> {
    let conn = app.db.conn().expect("conn");
    messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat))
        .order_by_asc(messages::Column::CreatedAt)
        .order_by_asc(messages::Column::Id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read messages")
}

/// Every `quota_usage` row of the user.
pub async fn quota_rows(app: &TestApp, tenant: Uuid, user: Uuid) -> Vec<quota_usage::Model> {
    let conn = app.db.conn().expect("conn");
    quota_usage::Entity::find()
        .filter(quota_usage::Column::TenantId.eq(tenant))
        .filter(quota_usage::Column::UserId.eq(user))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read quota rows")
}

/// The user's row of the current `period` and `bucket`.
pub async fn quota_row(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    period: Period,
    bucket: Bucket,
) -> Option<quota_usage::Model> {
    let start = period_starts(OffsetDateTime::now_utc()).start(period);
    quota_rows(app, tenant, user).await.into_iter().find(|r| {
        r.period_type == period.as_str() && r.bucket == bucket.as_str() && r.period_start == start
    })
}

/// Adds `spent` credits to the user's current `period` / `bucket` row.
pub async fn seed_spent(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    period: Period,
    bucket: Bucket,
    spent: i64,
) {
    let key = BucketKey {
        tenant_id: tenant,
        user_id: user,
        period,
        period_start: period_starts(OffsetDateTime::now_utc()).start(period),
        bucket,
    };
    let delta = BucketDelta {
        spent_credits_micro: spent,
        ..BucketDelta::default()
    };
    let conn = app.db.conn().expect("conn");
    quota_repo::add(&conn, &key, &delta)
        .await
        .expect("seed quota usage");
}

/// Gives `chat` a provider vector store (`vs_…`), so `file_search` is part of its requests.
pub async fn seed_vector_store(app: &TestApp, tenant: Uuid, chat: Uuid) {
    let row = chat_vector_stores::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat),
        vector_store_id: Set(Some(format!("vs_{}", chat.simple()))),
        provider: Set("openai".into()),
        file_count: Set(0),
        created_at: Set(db_now()),
    };
    let conn = app.db.conn().expect("conn");
    secure_insert::<chat_vector_stores::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed vector store");
}

/// Inserts an assistant message of `(chat, request_id)` directly (it then conflicts with the
/// turn's own assistant message).
pub async fn seed_assistant_message(app: &TestApp, tenant: Uuid, chat: Uuid, request_id: Uuid) {
    let now = db_now();
    let row = messages::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat),
        request_id: Set(Some(request_id)),
        role: Set("assistant".into()),
        content: Set("seeded".into()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".into()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(None),
    };
    let conn = app.db.conn().expect("conn");
    secure_insert::<messages::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed assistant message");
}

/// Moves turn `id` to `state` / `error_code` directly, as another finalizer (the orphan
/// watchdog) would.
pub async fn set_turn_state(app: &TestApp, id: Uuid, state: &str, error_code: Option<&str>) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::State, Expr::value(state))
        .col_expr(
            chat_turns::Column::ErrorCode,
            Expr::value(error_code.map(str::to_owned)),
        )
        .filter(chat_turns::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("set turn state");
}

/// Every `message_attachments` row of `chat`.
pub async fn message_attachments_of(app: &TestApp, chat: Uuid) -> Vec<message_attachments::Model> {
    let conn = app.db.conn().expect("conn");
    message_attachments::Entity::find()
        .filter(message_attachments::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read message attachments")
}

/// Soft-deletes attachment `id` (`deleted_at = now`).
pub async fn soft_delete_attachment(app: &TestApp, id: Uuid) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    attachments::Entity::update_many()
        .col_expr(attachments::Column::DeletedAt, Expr::value(db_now()))
        .filter(attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("soft-delete attachment");
}

/// Every `message_reactions` row of message `message_id`.
pub async fn reactions_of(app: &TestApp, message_id: Uuid) -> Vec<message_reactions::Model> {
    let conn = app.db.conn().expect("conn");
    message_reactions::Entity::find()
        .filter(message_reactions::Column::MessageId.eq(message_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read reactions")
}

/// Inserts the chat's thread summary with the inclusive frontier `frontier` (a message).
pub async fn seed_thread_summary(app: &TestApp, frontier: &messages::Model) {
    let now = db_now();
    let row = thread_summaries::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(frontier.tenant_id),
        chat_id: Set(frontier.chat_id),
        summary_text: Set("earlier conversation".into()),
        summarized_up_to_created_at: Set(frontier.created_at),
        summarized_up_to_message_id: Set(frontier.id),
        token_estimate: Set(5),
        created_at: Set(now),
        updated_at: Set(now),
    };
    let conn = app.db.conn().expect("conn");
    secure_insert::<thread_summaries::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed thread summary");
}

/// The thread summary row of `chat`, if any.
pub async fn thread_summary_of(app: &TestApp, chat: Uuid) -> Option<thread_summaries::Model> {
    let conn = app.db.conn().expect("conn");
    thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .expect("read thread summary")
}

/// Sets `is_compressed` on every message of `chat`, as a committed summary would.
pub async fn compress_messages(app: &TestApp, chat: Uuid) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(true))
        .filter(messages::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("compress messages");
}

/// Rewrites `requester_user_id` of turn `id` (a turn requested by someone else).
pub async fn set_turn_requester(app: &TestApp, id: Uuid, user: Uuid) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = app.db.conn().expect("conn");
    chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::RequesterUserId, Expr::value(Some(user)))
        .filter(chat_turns::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("set turn requester");
}
