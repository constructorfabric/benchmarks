//! Repository helpers over the Secure ORM. Every query is scoped: chat
//! queries by the PEP scope (tenant + owner), child tables by the tenant part
//! of that scope plus the chat id obtained from a scoped chat query, and
//! background workers by `AccessScope::allow_all()` with explicit tenant
//! predicates.

use sea_orm::EntityTrait;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{ColumnTrait, Condition, DbErr, Order, Set};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult};
use crate::domain::quota::{BucketUsage, Periods, UsageSnapshot};
use crate::infra::db::entities::{
    attachments, chat_turns, chat_vector_stores, chats, message_attachments, message_reactions,
    messages, quota_usage, thread_summaries,
};

pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETED: &str = "completed";
pub const STATE_FAILED: &str = "failed";
pub const STATE_CANCELLED: &str = "cancelled";

#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

// ───────────────────────────── chats ─────────────────────────────

pub async fn find_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chats::Model>> {
    Ok(chats::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chats::Column::Id.eq(chat_id))
                .add(chats::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?)
}

pub async fn require_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<chats::Model> {
    find_chat(runner, scope, chat_id)
        .await?
        .ok_or_else(|| DomainError::chat_not_found(chat_id))
}

pub async fn touch_chat(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> DomainResult<()> {
    chats::Entity::update_many()
        .secure()
        .col_expr(chats::Column::UpdatedAt, Expr::value(at))
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

pub async fn count_messages(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<i64> {
    let n = messages::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

// ───────────────────────────── messages ─────────────────────────────

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub role: &'static str,
    pub content: String,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub provider_response_id: Option<String>,
    pub created_at: OffsetDateTime,
}

pub async fn insert_message(
    runner: &impl DBRunner,
    scope: &AccessScope,
    m: NewMessage,
) -> DomainResult<messages::Model> {
    let am = messages::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(Some(m.request_id)),
        role: Set(m.role.to_owned()),
        content: Set(m.content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(m.provider_response_id),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(m.input_tokens),
        output_tokens: Set(m.output_tokens),
        cache_read_input_tokens: Set(m.cache_read_input_tokens),
        cache_write_input_tokens: Set(m.cache_write_input_tokens),
        reasoning_tokens: Set(m.reasoning_tokens),
        model: Set(m.model),
        is_compressed: Set(false),
        created_at: Set(m.created_at),
        deleted_at: Set(None),
    };
    Ok(secure_insert::<messages::Entity>(am, scope, runner).await?)
}

/// Non-deleted messages of a chat in `(created_at, id)` order.
pub async fn list_live_messages(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Vec<messages::Model>> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .order_by(messages::Column::CreatedAt, Order::Asc)
        .order_by(messages::Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

pub async fn find_message(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> DomainResult<Option<messages::Model>> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(messages::Column::Id.eq(message_id))
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?)
}

pub async fn messages_of_request(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Vec<messages::Model>> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(request_id)),
        )
        .order_by(messages::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// Soft-delete the live messages of a request id.
pub async fn soft_delete_request_messages(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    at: OffsetDateTime,
) -> DomainResult<()> {
    messages::Entity::update_many()
        .secure()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(at)))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// `input_tokens + output_tokens` of the most recent live assistant message
/// with non-zero usage.
pub async fn prior_context_tokens(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<i64> {
    let m = messages::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(messages::Column::Role.eq("assistant"))
                .add(
                    Condition::any()
                        .add(messages::Column::InputTokens.gt(0))
                        .add(messages::Column::OutputTokens.gt(0)),
                ),
        )
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(m.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

pub async fn set_compressed_all(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    value: bool,
) -> DomainResult<()> {
    messages::Entity::update_many()
        .secure()
        .col_expr(messages::Column::IsCompressed, Expr::value(value))
        .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)))
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

pub async fn set_compressed_ids(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    ids: &[Uuid],
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    messages::Entity::update_many()
        .secure()
        .col_expr(messages::Column::IsCompressed, Expr::value(true))
        .filter(Condition::all().add(messages::Column::Id.is_in(ids.to_vec())))
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

// ───────────────────────────── turns ─────────────────────────────

pub async fn find_turn_by_request(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Option<chat_turns::Model>> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .one(runner)
        .await?)
}

pub async fn find_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
) -> DomainResult<Option<chat_turns::Model>> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
        .one(runner)
        .await?)
}

pub async fn running_turn(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turns::Model>> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq(STATE_RUNNING))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?)
}

/// Latest live turn by `(started_at, id)`.
pub async fn latest_turn(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turns::Model>> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turns::Column::StartedAt, Order::Desc)
        .order_by(chat_turns::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Preflight columns of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Preflight<'a> {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: &'a str,
    pub minimal_generation_floor_applied: i64,
}

#[derive(Debug, Clone)]
pub struct NewTurn {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Uuid,
    pub web_search_enabled: bool,
    pub started_at: OffsetDateTime,
}

pub async fn insert_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    t: &NewTurn,
    pre: Option<Preflight<'_>>,
) -> DomainResult<chat_turns::Model> {
    let am = chat_turns::ActiveModel {
        id: Set(t.id),
        tenant_id: Set(t.tenant_id),
        chat_id: Set(t.chat_id),
        request_id: Set(t.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(t.requester_user_id)),
        state: Set(STATE_RUNNING.to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        error_detail: Set(None),
        reserve_tokens: Set(pre.map(|p| p.reserve_tokens)),
        max_output_tokens_applied: Set(pre.map(|p| i32::try_from(p.max_output_tokens_applied).unwrap_or(i32::MAX))),
        reserved_credits_micro: Set(pre.map(|p| p.reserved_credits_micro)),
        policy_version_applied: Set(pre.map(|p| p.policy_version_applied)),
        effective_model: Set(pre.map(|p| p.effective_model.to_owned())),
        minimal_generation_floor_applied: Set(
            pre.map(|p| i32::try_from(p.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
        ),
        deleted_at: Set(None),
        replaced_by_request_id: Set(None),
        started_at: Set(t.started_at),
        last_progress_at: Set(Some(t.started_at)),
        web_search_enabled: Set(t.web_search_enabled),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(t.started_at),
    };
    Ok(secure_insert::<chat_turns::Entity>(am, scope, runner).await?)
}

/// Fill the preflight columns of a retry/edit turn (only while NULL).
pub async fn set_turn_preflight(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    turn_id: Uuid,
    pre: Preflight<'_>,
) -> DomainResult<()> {
    chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::ReserveTokens, Expr::value(Some(pre.reserve_tokens)))
        .col_expr(
            chat_turns::Column::MaxOutputTokensApplied,
            Expr::value(Some(i32::try_from(pre.max_output_tokens_applied).unwrap_or(i32::MAX))),
        )
        .col_expr(chat_turns::Column::ReservedCreditsMicro, Expr::value(Some(pre.reserved_credits_micro)))
        .col_expr(chat_turns::Column::PolicyVersionApplied, Expr::value(Some(pre.policy_version_applied)))
        .col_expr(chat_turns::Column::EffectiveModel, Expr::value(Some(pre.effective_model.to_owned())))
        .col_expr(
            chat_turns::Column::MinimalGenerationFloorApplied,
            Expr::value(Some(i32::try_from(pre.minimal_generation_floor_applied).unwrap_or(i32::MAX))),
        )
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::ReserveTokens.is_null()),
        )
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Terminal CAS fields.
#[derive(Debug, Clone, Default)]
pub struct TerminalUpdate {
    pub state: &'static str,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub web_search_completed_count: Option<i32>,
    pub code_interpreter_completed_count: Option<i32>,
    pub file_search_completed_count: Option<i32>,
}

/// `UPDATE chat_turns SET … WHERE id = ? AND state = 'running'`. Returns
/// `true` for the CAS winner.
pub async fn cas_finalize_turn(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    turn_id: Uuid,
    u: &TerminalUpdate,
    at: OffsetDateTime,
) -> DomainResult<bool> {
    let mut q = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value(u.state))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(at)))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
        .col_expr(chat_turns::Column::ErrorCode, Expr::value(u.error_code.clone()))
        .col_expr(chat_turns::Column::ErrorDetail, Expr::value(u.error_detail.clone()))
        .col_expr(chat_turns::Column::AssistantMessageId, Expr::value(u.assistant_message_id));
    if let Some(rid) = &u.provider_response_id {
        q = q.col_expr(chat_turns::Column::ProviderResponseId, Expr::value(Some(rid.clone())));
    }
    if let Some(n) = u.web_search_completed_count {
        q = q.col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(n));
    }
    if let Some(n) = u.code_interpreter_completed_count {
        q = q.col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(n));
    }
    if let Some(n) = u.file_search_completed_count {
        q = q.col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(n));
    }
    let res = q
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(STATE_RUNNING)),
        )
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Progress heartbeat and tool counters of a running turn.
pub async fn update_turn_progress(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    turn_id: Uuid,
    at: OffsetDateTime,
    counters: (i32, i32, i32),
) -> DomainResult<()> {
    chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(at)))
        .col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(counters.0))
        .col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(counters.1))
        .col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(counters.2))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(STATE_RUNNING)),
        )
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(())
}

pub async fn soft_delete_turn(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    turn_id: Uuid,
    replaced_by: Option<Uuid>,
    at: OffsetDateTime,
) -> DomainResult<bool> {
    let res = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(Some(at)))
        .col_expr(chat_turns::Column::ReplacedByRequestId, Expr::value(replaced_by))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(chat_turns::Column::State.ne(STATE_RUNNING)),
        )
        .scope_with(tenant_scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

// ───────────────────────────── attachments ─────────────────────────────

pub async fn chat_attachments(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Vec<attachments::Model>> {
    Ok(attachments::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .order_by(attachments::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

pub async fn find_attachment(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> DomainResult<Option<attachments::Model>> {
    Ok(attachments::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(attachments::Column::Id.eq(attachment_id))
                .add(attachments::Column::ChatId.eq(chat_id)),
        )
        .one(runner)
        .await?)
}

pub async fn attachments_by_ids(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    ids: &[Uuid],
) -> DomainResult<Vec<attachments::Model>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachments::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(Condition::all().add(attachments::Column::Id.is_in(ids.to_vec())))
        .all(runner)
        .await?)
}

pub async fn insert_message_attachments(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
    at: OffsetDateTime,
) -> DomainResult<()> {
    for aid in attachment_ids {
        let am = message_attachments::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*aid),
            created_at: Set(at),
        };
        message_attachments::Entity::insert(am)
            .secure()
            .scope_unchecked(scope)?
            .exec(runner)
            .await?;
    }
    Ok(())
}

pub async fn message_attachment_links(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<Vec<message_attachments::Model>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(message_attachments::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .order_by(message_attachments::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// `true` when any submitted message references the attachment.
pub async fn attachment_referenced(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> DomainResult<bool> {
    let n = message_attachments::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::AttachmentId.eq(attachment_id)),
        )
        .count(runner)
        .await?;
    Ok(n > 0)
}

// ───────────────────────────── vector stores ─────────────────────────────

pub async fn find_vector_store(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> DomainResult<Option<chat_vector_stores::Model>> {
    Ok(chat_vector_stores::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(chat_vector_stores::Column::TenantId.eq(tenant_id))
                .add(chat_vector_stores::Column::ChatId.eq(chat_id)),
        )
        .one(runner)
        .await?)
}

pub async fn delete_vector_store_row(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    row_id: Uuid,
) -> DomainResult<()> {
    chat_vector_stores::Entity::delete_many()
        .secure()
        .scope_with(tenant_scope)
        .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
        .exec(runner)
        .await?;
    Ok(())
}

// ───────────────────────────── summaries ─────────────────────────────

pub async fn find_summary(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<thread_summaries::Model>> {
    Ok(thread_summaries::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .one(runner)
        .await?)
}

pub async fn delete_summary(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<()> {
    thread_summaries::Entity::delete_many()
        .secure()
        .scope_with(tenant_scope)
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .exec(runner)
        .await?;
    Ok(())
}

// ───────────────────────────── reactions ─────────────────────────────

pub async fn reactions_for(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> DomainResult<Vec<message_reactions::Model>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(message_reactions::Entity::find()
        .secure()
        .scope_with(tenant_scope)
        .filter(
            Condition::all()
                .add(message_reactions::Column::UserId.eq(user_id))
                .add(message_reactions::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .all(runner)
        .await?)
}

// ───────────────────────────── quota ─────────────────────────────

fn quota_key(tenant_id: Uuid, user_id: Uuid, period: &str, start: Date, bucket: &str) -> Condition {
    Condition::all()
        .add(quota_usage::Column::TenantId.eq(tenant_id))
        .add(quota_usage::Column::UserId.eq(user_id))
        .add(quota_usage::Column::PeriodType.eq(period))
        .add(quota_usage::Column::PeriodStart.eq(start))
        .add(quota_usage::Column::Bucket.eq(bucket))
}

pub async fn read_bucket(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: &str,
    start: Date,
    bucket: &str,
) -> DomainResult<BucketUsage> {
    let row = quota_usage::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(quota_key(tenant_id, user_id, period, start, bucket))
        .one(runner)
        .await?;
    Ok(row.map_or_else(BucketUsage::default, |r| BucketUsage {
        spent: r.spent_credits_micro,
        reserved: r.reserved_credits_micro,
        web_search_calls: i64::from(r.web_search_calls),
        code_interpreter_calls: i64::from(r.code_interpreter_calls),
    }))
}

pub async fn read_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: Periods,
) -> DomainResult<UsageSnapshot> {
    use crate::domain::quota::{BUCKET_PREMIUM, BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY};
    Ok(UsageSnapshot {
        total_daily: read_bucket(runner, tenant_id, user_id, PERIOD_DAILY, periods.daily, BUCKET_TOTAL).await?,
        total_monthly: read_bucket(runner, tenant_id, user_id, PERIOD_MONTHLY, periods.monthly, BUCKET_TOTAL)
            .await?,
        premium_daily: read_bucket(runner, tenant_id, user_id, PERIOD_DAILY, periods.daily, BUCKET_PREMIUM)
            .await?,
        premium_monthly: read_bucket(runner, tenant_id, user_id, PERIOD_MONTHLY, periods.monthly, BUCKET_PREMIUM)
            .await?,
    })
}

fn is_not_inserted(e: &ScopeError) -> bool {
    matches!(e, ScopeError::Db(DbErr::RecordNotInserted))
}

/// Ensure a bucket row exists (`INSERT … ON CONFLICT DO NOTHING`).
pub async fn ensure_bucket(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: &str,
    start: Date,
    bucket: &str,
) -> DomainResult<()> {
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(period.to_owned()),
        period_start: Set(start),
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
        updated_at: Set(now()),
    };
    let on_conflict = OnConflict::columns([
        quota_usage::Column::TenantId,
        quota_usage::Column::UserId,
        quota_usage::Column::PeriodType,
        quota_usage::Column::PeriodStart,
        quota_usage::Column::Bucket,
    ])
    .do_nothing()
    .to_owned();
    let res = quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::allow_all())?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await;
    match res {
        Ok(_) => Ok(()),
        Err(e) if is_not_inserted(&e) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Signed increments applied to one bucket row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketDelta {
    pub reserved: i64,
    pub spent: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

pub async fn apply_bucket_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: &str,
    start: Date,
    bucket: &str,
    d: BucketDelta,
) -> DomainResult<()> {
    ensure_bucket(runner, tenant_id, user_id, period, start, bucket).await?;
    let mut q = quota_usage::Entity::update_many()
        .secure()
        .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()));
    let add = |col: quota_usage::Column, v: i64| Expr::col(col).add(v);
    if d.reserved != 0 {
        q = q.col_expr(
            quota_usage::Column::ReservedCreditsMicro,
            add(quota_usage::Column::ReservedCreditsMicro, d.reserved),
        );
    }
    if d.spent != 0 {
        q = q.col_expr(
            quota_usage::Column::SpentCreditsMicro,
            add(quota_usage::Column::SpentCreditsMicro, d.spent),
        );
    }
    if d.calls != 0 {
        q = q.col_expr(quota_usage::Column::Calls, add(quota_usage::Column::Calls, d.calls));
    }
    if d.input_tokens != 0 {
        q = q.col_expr(quota_usage::Column::InputTokens, add(quota_usage::Column::InputTokens, d.input_tokens));
    }
    if d.output_tokens != 0 {
        q = q.col_expr(quota_usage::Column::OutputTokens, add(quota_usage::Column::OutputTokens, d.output_tokens));
    }
    if d.web_search_calls != 0 {
        q = q.col_expr(
            quota_usage::Column::WebSearchCalls,
            add(quota_usage::Column::WebSearchCalls, d.web_search_calls),
        );
    }
    if d.code_interpreter_calls != 0 {
        q = q.col_expr(
            quota_usage::Column::CodeInterpreterCalls,
            add(quota_usage::Column::CodeInterpreterCalls, d.code_interpreter_calls),
        );
    }
    q.filter(quota_key(tenant_id, user_id, period, start, bucket))
        .scope_with(&AccessScope::allow_all())
        .exec(runner)
        .await?;
    Ok(())
}

/// Insert a chat and return it.
pub async fn insert_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    am: chats::ActiveModel,
) -> DomainResult<chats::Model> {
    Ok(secure_insert::<chats::Entity>(am, scope, runner).await?)
}

/// `true` when `Err` is a unique violation on a turn insert.
#[must_use]
pub fn is_unique(e: &DomainError) -> bool {
    e.is_unique_violation()
}
