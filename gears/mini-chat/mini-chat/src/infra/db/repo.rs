//! Repository functions over the Secure ORM.
//!
//! Chats and quota rows are queried with the PEP scope (tenant + owner). Child
//! rows of a chat (messages, turns, attachments, ...) are queried with a tenant
//! scope plus the `chat_id` of a chat loaded under the PEP scope.

use std::collections::HashMap;

use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait, Order, QueryFilter, QuerySelect,
};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
    secure_insert,
};
use uuid::Uuid;

use super::entities::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, message_reaction,
    quota_usage, thread_summary,
};
use crate::domain::error::{DomainError, DomainResult};

fn cond() -> Condition {
    Condition::all()
}

// ───────────────────────────── chats ─────────────────────────────

pub async fn insert_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    m: chat::Model,
) -> DomainResult<chat::Model> {
    let am = chat::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        user_id: Set(m.user_id),
        model: Set(m.model),
        title: Set(m.title),
        is_temporary: Set(m.is_temporary),
        created_at: Set(m.created_at),
        updated_at: Set(m.updated_at),
        deleted_at: Set(m.deleted_at),
    };
    Ok(secure_insert::<chat::Entity>(am, scope, runner).await?)
}

pub async fn find_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> DomainResult<Option<chat::Model>> {
    Ok(chat::Entity::find()
        .filter(
            cond()
                .add(chat::Column::Id.eq(id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Chat by id without the owner scope (background workers; tenant from the payload).
pub async fn find_chat_any(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
) -> DomainResult<Option<chat::Model>> {
    Ok(chat::Entity::find()
        .filter(cond().add(chat::Column::Id.eq(id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

pub async fn touch_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(
            cond()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

pub async fn update_chat_title(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    title: String,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::Title, Expr::value(title))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(
            cond()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

pub async fn soft_delete_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = chat::Entity::update_many()
        .secure()
        .col_expr(chat::Column::DeletedAt, Expr::value(now))
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(
            cond()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Non-deleted message counts per chat.
pub async fn message_counts(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_ids: &[Uuid],
) -> DomainResult<HashMap<Uuid, i64>> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<ChatCount> = message::Entity::find()
        .filter(
            cond()
                .add(message::Column::ChatId.is_in(chat_ids.to_vec()))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .project_all(runner, |q| {
            q.select_only()
                .column(message::Column::ChatId)
                .column_as(Expr::col(message::Column::Id).count(), "cnt")
                .group_by(message::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

// ───────────────────────────── messages ─────────────────────────────

#[must_use]
pub fn message_model(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    role: &str,
    content: String,
    now: OffsetDateTime,
) -> message::Model {
    message::Model {
        id,
        tenant_id,
        chat_id,
        request_id: Some(request_id),
        role: role.to_owned(),
        content,
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: None,
        is_compressed: false,
        created_at: now,
        deleted_at: None,
    }
}

pub async fn insert_message(
    runner: &impl DBRunner,
    m: message::Model,
) -> DomainResult<message::Model> {
    let scope = AccessScope::for_tenant(m.tenant_id);
    let am = message::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(m.request_id),
        role: Set(m.role),
        content: Set(m.content),
        content_type: Set(m.content_type),
        token_estimate: Set(m.token_estimate),
        provider_response_id: Set(m.provider_response_id),
        request_kind: Set(m.request_kind),
        features_used: Set(m.features_used),
        input_tokens: Set(m.input_tokens),
        output_tokens: Set(m.output_tokens),
        cache_read_input_tokens: Set(m.cache_read_input_tokens),
        cache_write_input_tokens: Set(m.cache_write_input_tokens),
        reasoning_tokens: Set(m.reasoning_tokens),
        model: Set(m.model),
        is_compressed: Set(m.is_compressed),
        created_at: Set(m.created_at),
        deleted_at: Set(m.deleted_at),
    };
    Ok(secure_insert::<message::Entity>(am, &scope, runner).await?)
}

pub async fn find_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> DomainResult<Option<message::Model>> {
    Ok(message::Entity::find()
        .filter(
            cond()
                .add(message::Column::Id.eq(id))
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

/// Non-deleted messages of a turn (by `request_id`).
pub async fn turn_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Vec<message::Model>> {
    Ok(message::Entity::find()
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .all(runner)
        .await?)
}

/// Messages of a turn by `request_id`, deleted or not (latest first).
pub async fn turn_messages_any(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Vec<message::Model>> {
    Ok(message::Entity::find()
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .all(runner)
        .await?)
}

pub async fn soft_delete_turn_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = message::Entity::update_many()
        .secure()
        .col_expr(message::Column::DeletedAt, Expr::value(now))
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Latest non-deleted message of the chat, by `(created_at, id)`.
pub async fn latest_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    exclude_request_id: Option<Uuid>,
) -> DomainResult<Option<message::Model>> {
    let mut c = cond()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null());
    if let Some(rid) = exclude_request_id {
        c = c.add(
            Condition::any()
                .add(message::Column::RequestId.ne(rid))
                .add(message::Column::RequestId.is_null()),
        );
    }
    Ok(message::Entity::find()
        .filter(c)
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Most recent non-deleted assistant message with non-zero token counts.
pub async fn latest_assistant_with_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<message::Model>> {
    Ok(message::Entity::find()
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(message::Column::Role.eq("assistant"))
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.gt(0))
                        .add(message::Column::OutputTokens.gt(0)),
                ),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// `(created_at, id) > (t, id)` in the strict message order.
fn after(t: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(message::Column::CreatedAt.gt(t)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(t))
            .add(message::Column::Id.gt(id)),
    )
}

/// `(created_at, id) <= (t, id)` in the strict message order.
fn at_or_before(t: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(message::Column::CreatedAt.lt(t)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(t))
            .add(message::Column::Id.lte(id)),
    )
}

/// Recent messages for context assembly (DESIGN §4 "Recent messages query"), newest first.
pub async fn recent_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    boundary: Option<(OffsetDateTime, Uuid)>,
    frontier: Option<(OffsetDateTime, Uuid)>,
    limit: u64,
) -> DomainResult<Vec<message::Model>> {
    // No boundary means the chat had no earlier message: there is no history.
    let Some((bt, bid)) = boundary else {
        return Ok(Vec::new());
    };
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut c = cond()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::RequestId.is_not_null())
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(at_or_before(bt, bid));
    if let Some((t, id)) = frontier {
        c = c.add(after(t, id));
    }
    Ok(message::Entity::find()
        .filter(c)
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// Messages in `(base, target]` for a summary run, oldest first.
pub async fn summary_range(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> DomainResult<Vec<message::Model>> {
    let mut c = cond()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(at_or_before(target.0, target.1));
    if let Some((t, id)) = base {
        c = c.add(after(t, id));
    }
    Ok(message::Entity::find()
        .filter(c)
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

pub async fn mark_compressed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: Vec<Uuid>,
) -> DomainResult<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let r = message::Entity::update_many()
        .secure()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::Id.is_in(ids)),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

pub async fn clear_compressed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<u64> {
    let r = message::Entity::update_many()
        .secure()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(
            cond()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::IsCompressed.eq(true)),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

// ───────────────────────────── turns ─────────────────────────────

pub async fn insert_turn(
    runner: &impl DBRunner,
    m: chat_turn::Model,
) -> DomainResult<chat_turn::Model> {
    let scope = AccessScope::for_tenant(m.tenant_id);
    let am = chat_turn::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(m.request_id),
        requester_type: Set(m.requester_type),
        requester_user_id: Set(m.requester_user_id),
        state: Set(m.state),
        provider_name: Set(m.provider_name),
        provider_response_id: Set(m.provider_response_id),
        assistant_message_id: Set(m.assistant_message_id),
        error_code: Set(m.error_code),
        reserve_tokens: Set(m.reserve_tokens),
        max_output_tokens_applied: Set(m.max_output_tokens_applied),
        reserved_credits_micro: Set(m.reserved_credits_micro),
        policy_version_applied: Set(m.policy_version_applied),
        effective_model: Set(m.effective_model),
        minimal_generation_floor_applied: Set(m.minimal_generation_floor_applied),
        error_detail: Set(m.error_detail),
        deleted_at: Set(m.deleted_at),
        replaced_by_request_id: Set(m.replaced_by_request_id),
        started_at: Set(m.started_at),
        last_progress_at: Set(m.last_progress_at),
        web_search_enabled: Set(m.web_search_enabled),
        web_search_completed_count: Set(m.web_search_completed_count),
        code_interpreter_completed_count: Set(m.code_interpreter_completed_count),
        file_search_completed_count: Set(m.file_search_completed_count),
        completed_at: Set(m.completed_at),
        updated_at: Set(m.updated_at),
    };
    Ok(secure_insert::<chat_turn::Entity>(am, &scope, runner).await?)
}

/// Turn by `(chat_id, request_id)`, including soft-deleted turns.
pub async fn find_turn_by_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .filter(
            cond()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

pub async fn find_turn(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .filter(cond().add(chat_turn::Column::Id.eq(id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

/// The running (non-deleted) turn of a chat, if any.
pub async fn running_turn(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .filter(
            cond()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::State.eq("running"))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

/// Latest non-deleted turn by `(started_at, id)`.
pub async fn latest_turn(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .filter(
            cond()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Generic guarded update of a turn: applies `cols` when `extra` matches.
pub async fn update_turn_where(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    extra: Condition,
    cols: Vec<(chat_turn::Column, sea_orm::sea_query::SimpleExpr)>,
) -> DomainResult<u64> {
    let mut q = chat_turn::Entity::update_many().secure();
    for (c, e) in cols {
        q = q.col_expr(c, e);
    }
    let r = q
        .filter(cond().add(chat_turn::Column::Id.eq(turn_id)).add(extra))
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Orphan candidates: running, non-deleted, stale progress.
pub async fn orphan_candidates(
    runner: &impl DBRunner,
    cutoff: OffsetDateTime,
    limit: u64,
) -> DomainResult<Vec<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .filter(
            cond()
                .add(chat_turn::Column::State.eq("running"))
                .add(chat_turn::Column::DeletedAt.is_null())
                .add(stale_progress(cutoff)),
        )
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(chat_turn::Column::StartedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND started_at <= cutoff)`.
#[must_use]
pub fn stale_progress(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

// ───────────────────────────── attachments ─────────────────────────────

pub async fn insert_attachment(
    runner: &impl DBRunner,
    m: attachment::Model,
) -> DomainResult<attachment::Model> {
    let scope = AccessScope::for_tenant(m.tenant_id);
    let am = attachment::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        uploaded_by_user_id: Set(m.uploaded_by_user_id),
        filename: Set(m.filename),
        content_type: Set(m.content_type),
        size_bytes: Set(m.size_bytes),
        storage_backend: Set(m.storage_backend),
        provider_file_id: Set(m.provider_file_id),
        status: Set(m.status),
        error_code: Set(m.error_code),
        attachment_kind: Set(m.attachment_kind),
        for_file_search: Set(m.for_file_search),
        for_code_interpreter: Set(m.for_code_interpreter),
        doc_summary: Set(m.doc_summary),
        img_thumbnail: Set(m.img_thumbnail),
        img_thumbnail_width: Set(m.img_thumbnail_width),
        img_thumbnail_height: Set(m.img_thumbnail_height),
        summary_model: Set(m.summary_model),
        summary_updated_at: Set(m.summary_updated_at),
        cleanup_status: Set(m.cleanup_status),
        cleanup_attempts: Set(m.cleanup_attempts),
        last_cleanup_error: Set(m.last_cleanup_error),
        cleanup_updated_at: Set(m.cleanup_updated_at),
        created_at: Set(m.created_at),
        updated_at: Set(m.updated_at),
        deleted_at: Set(m.deleted_at),
        secondary_file_id: Set(m.secondary_file_id),
        secondary_status: Set(m.secondary_status),
        secondary_provider_kind: Set(m.secondary_provider_kind),
    };
    Ok(secure_insert::<attachment::Entity>(am, &scope, runner).await?)
}

/// Attachment by id within a chat, including soft-deleted rows.
pub async fn find_attachment_any(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> DomainResult<Option<attachment::Model>> {
    Ok(attachment::Entity::find()
        .filter(
            cond()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::ChatId.eq(chat_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

/// Attachment by id regardless of chat (background tasks).
pub async fn find_attachment_by_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
) -> DomainResult<Option<attachment::Model>> {
    Ok(attachment::Entity::find()
        .filter(cond().add(attachment::Column::Id.eq(id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

/// Non-deleted attachments of a chat with the given ids.
pub async fn attachments_by_ids(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: &[Uuid],
) -> DomainResult<Vec<attachment::Model>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachment::Entity::find()
        .filter(
            cond()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids.to_vec()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .all(runner)
        .await?)
}

/// All non-deleted attachments of a chat.
pub async fn chat_attachments(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Vec<attachment::Model>> {
    Ok(attachment::Entity::find()
        .filter(
            cond()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// All attachments of a chat including soft-deleted ones.
pub async fn chat_attachments_all(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Vec<attachment::Model>> {
    Ok(attachment::Entity::find()
        .filter(cond().add(attachment::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// Guarded update of an attachment.
pub async fn update_attachment_where(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    extra: Condition,
    cols: Vec<(attachment::Column, sea_orm::sea_query::SimpleExpr)>,
) -> DomainResult<u64> {
    let mut q = attachment::Entity::update_many().secure();
    for (c, e) in cols {
        q = q.col_expr(c, e);
    }
    let r = q
        .filter(cond().add(attachment::Column::Id.eq(id)).add(extra))
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Marks every attachment of a chat with no cleanup state as `pending` (chat soft delete).
pub async fn mark_chat_attachments_pending(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
        .filter(
            cond()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::CleanupStatus.is_null()),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Stale uploads for the upload reaper.
pub async fn stale_uploads(
    runner: &impl DBRunner,
    cutoff: OffsetDateTime,
    limit: u64,
) -> DomainResult<Vec<attachment::Model>> {
    Ok(attachment::Entity::find()
        .filter(
            cond()
                .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::UpdatedAt.lt(cutoff)),
        )
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(attachment::Column::UpdatedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

// ───────────────────────────── message_attachments ─────────────────────────────

pub async fn insert_message_attachment(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let am = message_attachment::ActiveModel {
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(now),
    };
    let scope = AccessScope::for_tenant(tenant_id);
    match message_attachment::Entity::insert(am)
        .secure()
        .scope_unchecked(&scope)?
        .exec(runner)
        .await
    {
        // Already linked (`RecordNotInserted`) is success too.
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Attachment ids linked to a message.
pub async fn message_attachment_ids(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
) -> DomainResult<Vec<Uuid>> {
    let rows = message_attachment::Entity::find()
        .filter(
            cond()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.eq(message_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?;
    Ok(rows.into_iter().map(|r| r.attachment_id).collect())
}

/// Links of many messages: message id -> attachment ids.
pub async fn message_attachment_links(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<Vec<message_attachment::Model>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(message_attachment::Entity::find()
        .filter(
            cond()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// `true` when any message references the attachment.
pub async fn attachment_referenced(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> DomainResult<bool> {
    let n = message_attachment::Entity::find()
        .filter(
            cond()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .count(runner)
        .await?;
    Ok(n > 0)
}

// ───────────────────────────── reactions ─────────────────────────────

pub async fn upsert_reaction(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    reaction: &str,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let am = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(message_id),
        user_id: Set(user_id),
        tenant_id: Set(tenant_id),
        reaction: Set(reaction.to_owned()),
        created_at: Set(now),
    };
    let scope = AccessScope::for_tenant(tenant_id);
    let on_conflict = OnConflict::columns([
        message_reaction::Column::MessageId,
        message_reaction::Column::UserId,
    ])
    .update_columns([
        message_reaction::Column::Reaction,
        message_reaction::Column::CreatedAt,
    ])
    .to_owned();
    message_reaction::Entity::insert(am)
        .secure()
        .scope_unchecked(&scope)?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await?;
    Ok(())
}

pub async fn find_reaction(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
) -> DomainResult<Option<message_reaction::Model>> {
    Ok(message_reaction::Entity::find()
        .filter(
            cond()
                .add(message_reaction::Column::MessageId.eq(message_id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

pub async fn delete_reaction(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
) -> DomainResult<u64> {
    let r = message_reaction::Entity::delete_many()
        .filter(
            cond()
                .add(message_reaction::Column::MessageId.eq(message_id))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Reactions of the user on many messages.
pub async fn reactions_for(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<HashMap<Uuid, String>> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_reaction::Entity::find()
        .filter(
            cond()
                .add(message_reaction::Column::MessageId.is_in(message_ids.to_vec()))
                .add(message_reaction::Column::UserId.eq(user_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .all(runner)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.message_id, r.reaction))
        .collect())
}

// ───────────────────────────── thread summaries ─────────────────────────────

pub async fn find_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<thread_summary::Model>> {
    Ok(thread_summary::Entity::find()
        .filter(cond().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

pub async fn delete_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<u64> {
    let r = thread_summary::Entity::delete_many()
        .filter(cond().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

pub async fn insert_summary(runner: &impl DBRunner, m: thread_summary::Model) -> DomainResult<()> {
    let scope = AccessScope::for_tenant(m.tenant_id);
    let am = thread_summary::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        summary_text: Set(m.summary_text),
        summarized_up_to_created_at: Set(m.summarized_up_to_created_at),
        summarized_up_to_message_id: Set(m.summarized_up_to_message_id),
        token_estimate: Set(m.token_estimate),
        created_at: Set(m.created_at),
        updated_at: Set(m.updated_at),
    };
    secure_insert::<thread_summary::Entity>(am, &scope, runner).await?;
    Ok(())
}

/// CAS update of the summary on its stored frontier.
#[allow(clippy::too_many_arguments)]
pub async fn cas_update_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: (OffsetDateTime, Uuid),
    text: String,
    target: (OffsetDateTime, Uuid),
    token_estimate: i32,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let r = thread_summary::Entity::update_many()
        .secure()
        .col_expr(thread_summary::Column::SummaryText, Expr::value(text))
        .col_expr(
            thread_summary::Column::SummarizedUpToCreatedAt,
            Expr::value(target.0),
        )
        .col_expr(
            thread_summary::Column::SummarizedUpToMessageId,
            Expr::value(target.1),
        )
        .col_expr(
            thread_summary::Column::TokenEstimate,
            Expr::value(token_estimate),
        )
        .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
        .filter(
            cond()
                .add(thread_summary::Column::ChatId.eq(chat_id))
                .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(base.0))
                .add(thread_summary::Column::SummarizedUpToMessageId.eq(base.1)),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

// ───────────────────────────── vector stores ─────────────────────────────

pub async fn find_vector_store(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<chat_vector_store::Model>> {
    Ok(chat_vector_store::Entity::find()
        .filter(cond().add(chat_vector_store::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(runner)
        .await?)
}

pub async fn insert_vector_store_placeholder(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    provider: &str,
    now: OffsetDateTime,
) -> DomainResult<Uuid> {
    let id = Uuid::new_v4();
    let am = chat_vector_store::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        vector_store_id: Set(None),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(now),
    };
    secure_insert::<chat_vector_store::Entity>(am, &AccessScope::for_tenant(tenant_id), runner)
        .await?;
    Ok(id)
}

pub async fn set_vector_store_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    row_id: Uuid,
    vs_id: &str,
) -> DomainResult<u64> {
    let r = chat_vector_store::Entity::update_many()
        .secure()
        .col_expr(
            chat_vector_store::Column::VectorStoreId,
            Expr::value(vs_id.to_owned()),
        )
        .filter(
            cond()
                .add(chat_vector_store::Column::Id.eq(row_id))
                .add(chat_vector_store::Column::VectorStoreId.is_null()),
        )
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

pub async fn delete_vector_store_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    row_id: Uuid,
) -> DomainResult<u64> {
    let r = chat_vector_store::Entity::delete_many()
        .filter(cond().add(chat_vector_store::Column::Id.eq(row_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Deletes a stale placeholder (still without a `vector_store_id`).
pub async fn delete_vector_store_placeholder(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    row_id: Uuid,
) -> DomainResult<u64> {
    let r = chat_vector_store::Entity::delete_many()
        .filter(
            cond()
                .add(chat_vector_store::Column::Id.eq(row_id))
                .add(chat_vector_store::Column::VectorStoreId.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

// ───────────────────────────── quota usage ─────────────────────────────

/// Quota scope of one user.
#[must_use]
pub fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

pub async fn quota_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &[(&str, Date)],
) -> DomainResult<Vec<quota_usage::Model>> {
    let mut any = Condition::any();
    for (pt, ps) in periods {
        any = any.add(
            cond()
                .add(quota_usage::Column::PeriodType.eq(*pt))
                .add(quota_usage::Column::PeriodStart.eq(*ps)),
        );
    }
    Ok(quota_usage::Entity::find()
        .filter(
            cond()
                .add(quota_usage::Column::TenantId.eq(tenant_id))
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(any),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Ensures a bucket row exists.
pub async fn ensure_quota_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period_type: &str,
    period_start: Date,
    bucket: &str,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(period_type.to_owned()),
        period_start: Set(period_start),
        bucket: Set(bucket.to_owned()),
        spent_credits_micro: Set(0),
        reserved_credits_micro: Set(0),
        calls: Set(0),
        input_tokens: Set(0),
        output_tokens: Set(0),
        file_search_calls: Set(0),
        web_search_calls: Set(0),
        code_interpreter_calls: Set(0),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(now),
    };
    let scope = user_scope(tenant_id, user_id);
    let on_conflict = OnConflict::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
    .do_nothing()
    .to_owned();
    match quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(&scope)?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await
    {
        Ok(_) | Err(toolkit_db::secure::ScopeError::Db(DbErr::RecordNotInserted)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Applies increments to one bucket row.
#[allow(clippy::too_many_arguments)]
pub async fn bump_quota_row(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period_type: &str,
    period_start: Date,
    bucket: &str,
    deltas: &QuotaDeltas,
    now: OffsetDateTime,
) -> DomainResult<u64> {
    let mut q = quota_usage::Entity::update_many().secure();
    let add = |c: quota_usage::Column, v: i64| (c, Expr::col(c).add(v));
    let mut cols = vec![(quota_usage::Column::UpdatedAt, Expr::value(now))];
    if deltas.reserved != 0 {
        cols.push(add(
            quota_usage::Column::ReservedCreditsMicro,
            deltas.reserved,
        ));
    }
    if deltas.spent != 0 {
        cols.push(add(quota_usage::Column::SpentCreditsMicro, deltas.spent));
    }
    if deltas.calls != 0 {
        cols.push(add(quota_usage::Column::Calls, deltas.calls));
    }
    if deltas.input_tokens != 0 {
        cols.push(add(quota_usage::Column::InputTokens, deltas.input_tokens));
    }
    if deltas.output_tokens != 0 {
        cols.push(add(quota_usage::Column::OutputTokens, deltas.output_tokens));
    }
    if deltas.web_search_calls != 0 {
        cols.push(add(
            quota_usage::Column::WebSearchCalls,
            deltas.web_search_calls,
        ));
    }
    if deltas.code_interpreter_calls != 0 {
        cols.push(add(
            quota_usage::Column::CodeInterpreterCalls,
            deltas.code_interpreter_calls,
        ));
    }
    for (c, e) in cols {
        q = q.col_expr(c, e);
    }
    let r = q
        .filter(
            cond()
                .add(quota_usage::Column::TenantId.eq(tenant_id))
                .add(quota_usage::Column::UserId.eq(user_id))
                .add(quota_usage::Column::PeriodType.eq(period_type))
                .add(quota_usage::Column::PeriodStart.eq(period_start))
                .add(quota_usage::Column::Bucket.eq(bucket)),
        )
        .scope_with(&user_scope(tenant_id, user_id))
        .exec(runner)
        .await?;
    Ok(r.rows_affected)
}

/// Increments applied to a bucket row.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaDeltas {
    pub reserved: i64,
    pub spent: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

#[allow(dead_code)]
fn _assert_internal(_: DomainError) {}
