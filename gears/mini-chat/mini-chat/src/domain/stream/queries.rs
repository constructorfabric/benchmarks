//! Turn / message / attachment queries used by the streaming and turn services. All queries are
//! tenant-scoped and chat-scoped (the chat was loaded with the caller's owner scope first).

use std::collections::{HashMap, HashSet};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, Resource};
use crate::infra::db::entities::{attachment, chat_turn, chat_vector_store, message, message_attachment};

pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETED: &str = "completed";
pub const STATE_FAILED: &str = "failed";
pub const STATE_CANCELLED: &str = "cancelled";

fn tenant(t: Uuid) -> AccessScope {
    AccessScope::for_tenant(t)
}

/// Turn by `(chat_id, request_id)` (soft-deleted included).
///
/// # Errors
/// DB errors.
pub async fn find_turn(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(chat_turn::Column::ChatId.eq(chat_id)).add(chat_turn::Column::RequestId.eq(request_id)))
        .one(db)
        .await?)
}

/// Turn by id.
///
/// # Errors
/// DB errors.
pub async fn turn_by_id(db: &impl DBRunner, tenant_id: Uuid, turn_id: Uuid) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
        .one(db)
        .await?)
}

/// `true` when a non-deleted running turn exists in the chat.
///
/// # Errors
/// DB errors.
pub async fn has_running_turn(db: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
    let n = chat_turn::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::State.eq(STATE_RUNNING))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .count(db)
        .await?;
    Ok(n > 0)
}

/// Latest non-deleted turn of the chat (greatest `(started_at, id)`).
///
/// # Errors
/// DB errors.
pub async fn latest_turn(db: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_turn::Model>, DomainError> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(chat_turn::Column::ChatId.eq(chat_id)).add(chat_turn::Column::DeletedAt.is_null()))
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(db)
        .await?)
}

/// Snapshot boundary: `(created_at, id)` of the latest non-deleted message of the chat.
///
/// # Errors
/// DB errors.
pub async fn snapshot_boundary(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<(OffsetDateTime, Uuid)>, DomainError> {
    let m = message::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)).add(message::Column::DeletedAt.is_null()))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(db)
        .await?;
    Ok(m.map(|m| (m.created_at, m.id)))
}

/// `input_tokens + output_tokens` of the latest non-deleted assistant message with usage.
///
/// # Errors
/// DB errors.
pub async fn prior_context_tokens(db: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    let m = message::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(message::Column::Role.eq("assistant"))
                .add(Condition::any().add(message::Column::InputTokens.gt(0)).add(message::Column::OutputTokens.gt(0))),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(db)
        .await?;
    Ok(m.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

/// Attachment state of a chat relevant to tool assembly and citations.
#[derive(Debug, Clone, Default)]
pub struct ChatAttachmentState {
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter: bool,
    pub code_interpreter_file_ids: Vec<String>,
    pub vector_store_id: Option<String>,
    /// `provider_file_id → (attachment_id, filename)` of ready, non-deleted attachments.
    pub file_map: HashMap<String, (Uuid, String)>,
}

/// Loads the ready attachments and the vector store of the chat.
///
/// # Errors
/// DB errors.
pub async fn chat_attachment_state(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<ChatAttachmentState, DomainError> {
    let rows = attachment::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::Status.eq("ready")),
        )
        .order_by(attachment::Column::CreatedAt, Order::Asc)
        .all(db)
        .await?;
    let mut st = ChatAttachmentState::default();
    for a in rows {
        if a.for_file_search {
            st.has_ready_documents = true;
        }
        if let Some(fid) = &a.provider_file_id {
            if a.for_code_interpreter {
                st.has_ready_code_interpreter = true;
                st.code_interpreter_file_ids.push(fid.clone());
            }
            st.file_map.insert(fid.clone(), (a.id, a.filename.clone()));
        }
    }
    let vs = chat_vector_store::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
        .one(db)
        .await?;
    st.vector_store_id = vs.and_then(|v| v.vector_store_id);
    Ok(st)
}

/// Attachments of the chat with the given ids (no validation).
///
/// # Errors
/// DB errors.
pub async fn attachments_by_ids(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachment::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(attachment::Column::ChatId.eq(chat_id)).add(attachment::Column::Id.is_in(ids.to_vec())))
        .all(db)
        .await?)
}

/// 400 `invalid_argument`, `field_violations[attachment].reason = invalid_attachment`.
#[must_use]
pub fn invalid_attachment(detail: impl Into<String>) -> DomainError {
    DomainError::invalid(Resource::Chat, "attachment", "invalid_attachment", detail)
}

/// Validates `attachment_ids` (same tenant, chat and uploader; ready; not deleted) and links them to
/// the user message. Returns the attachments in request order.
///
/// # Errors
/// `invalid_attachment` or DB errors.
pub async fn validate_and_link_attachments(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(invalid_attachment("duplicate attachment id"));
    }
    let rows = attachments_by_ids(tx, tenant_id, chat_id, ids).await?;
    let by_id: HashMap<Uuid, attachment::Model> = rows.into_iter().map(|a| (a.id, a)).collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(a) = by_id.get(id) else {
            return Err(invalid_attachment(format!("attachment {id} not found in chat")));
        };
        if a.deleted_at.is_some() || a.uploaded_by_user_id != user_id || a.status != "ready" || a.tenant_id != tenant_id {
            return Err(invalid_attachment(format!("attachment {id} is not available")));
        }
        out.push(a.clone());
    }
    link_attachments(tx, tenant_id, chat_id, message_id, &out.iter().map(|a| a.id).collect::<Vec<_>>(), now).await?;
    Ok(out)
}

/// Inserts `message_attachments` rows.
///
/// # Errors
/// DB errors.
pub async fn link_attachments(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use sea_orm::ActiveValue::Set;
    for id in attachment_ids {
        let am = message_attachment::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*id),
            created_at: Set(now),
        };
        toolkit_db::secure::secure_insert::<message_attachment::Entity>(am, &tenant(tenant_id), tx).await?;
    }
    Ok(())
}

/// User message of a turn (`role = user`, same `request_id`).
///
/// # Errors
/// DB errors.
pub async fn turn_user_message(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::Role.eq("user")),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .limit(1)
        .one(db)
        .await?)
}

/// Message by id within a chat.
///
/// # Errors
/// DB errors.
pub async fn message_by_id(db: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid, id: Uuid) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)).add(message::Column::Id.eq(id)))
        .one(db)
        .await?)
}

/// Non-deleted attachments linked to a message.
///
/// # Errors
/// DB errors.
pub async fn message_attachments(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    let links = message_attachment::Entity::find()
        .secure()
        .scope_with(&tenant(tenant_id))
        .filter(Condition::all().add(message_attachment::Column::ChatId.eq(chat_id)).add(message_attachment::Column::MessageId.eq(message_id)))
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .all(db)
        .await?;
    let ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let rows = attachments_by_ids(db, tenant_id, chat_id, &ids).await?;
    let by_id: HashMap<Uuid, attachment::Model> = rows.into_iter().map(|a| (a.id, a)).collect();
    Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).filter(|a| a.deleted_at.is_none()).collect())
}

/// Refreshes `last_progress_at` and the completed tool counters of a running turn.
///
/// # Errors
/// DB errors.
pub async fn update_progress(
    db: &impl DBRunner,
    tenant_id: Uuid,
    turn_id: Uuid,
    counts: (i32, i32, i32),
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
        .col_expr(chat_turn::Column::WebSearchCompletedCount, Expr::value(counts.0))
        .col_expr(chat_turn::Column::CodeInterpreterCompletedCount, Expr::value(counts.1))
        .col_expr(chat_turn::Column::FileSearchCompletedCount, Expr::value(counts.2))
        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)).add(chat_turn::Column::State.eq(STATE_RUNNING)))
        .secure()
        .scope_with(&tenant(tenant_id))
        .exec(db)
        .await?;
    Ok(())
}
