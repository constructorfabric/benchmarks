//! Chats, messages, reactions, turn status, models and quota status.

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::app::{App, fire, now, owner_scope, tenant_scope};
use super::authz::{actions, chat_scope, model_permission, quota_scope};
use super::error::DomainError;
use super::quota::{self, PeriodStarts, PeriodStatus};
use crate::infra::db::entity::{attachments, chat_turns, chats, message_attachments, message_reactions, messages};
use crate::infra::outbox::ChatCleanupPayload;

pub const PAGE_LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };

// ── OData fields ────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::UpdatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }

    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

pub struct ChatMapper;

impl FieldToColumn<ChatField> for ChatMapper {
    type Column = chats::Column;

    fn map_field(field: ChatField) -> chats::Column {
        match field {
            ChatField::UpdatedAt => chats::Column::UpdatedAt,
            ChatField::Id => chats::Column::Id,
            ChatField::Title => chats::Column::Title,
        }
    }
}

impl ODataFieldMapping<ChatField> for ChatMapper {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatField) -> sea_orm::Value {
        match field {
            ChatField::UpdatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatField::Id => sea_orm::Value::Uuid(Some(model.id)),
            ChatField::Title => sea_orm::Value::String(Some(model.title.clone().unwrap_or_default())),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageField {
    const FIELDS: &'static [Self] = &[Self::CreatedAt, Self::Id, Self::Role];

    fn name(&self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Id => "id",
            Self::Role => "role",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::CreatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

pub struct MessageMapper;

impl FieldToColumn<MessageField> for MessageMapper {
    type Column = messages::Column;

    fn map_field(field: MessageField) -> messages::Column {
        match field {
            MessageField::CreatedAt => messages::Column::CreatedAt,
            MessageField::Id => messages::Column::Id,
            MessageField::Role => messages::Column::Role,
        }
    }
}

impl ODataFieldMapping<MessageField> for MessageMapper {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageField) -> sea_orm::Value {
        match field {
            MessageField::CreatedAt => sea_orm::Value::TimeDateTimeWithTimeZone(Some(model.created_at)),
            MessageField::Id => sea_orm::Value::Uuid(Some(model.id)),
            MessageField::Role => sea_orm::Value::String(Some(model.role.clone())),
        }
    }
}

// ── Domain views ────────────────────────────────────────────────────────────

/// Chat with its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chats::Model,
    pub message_count: i64,
}

/// Attachment summary of a message.
#[derive(Debug, Clone)]
pub struct AttachmentSummary {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    pub thumbnail: Option<Thumbnail>,
}

#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// Message with its attachments and the caller's reaction.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message: messages::Model,
    pub request_id: Uuid,
    pub attachments: Vec<AttachmentSummary>,
    pub my_reaction: Option<String>,
}

/// Validates and trims a chat title.
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    if t.is_empty() || t.chars().count() > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

/// Loads a non-deleted chat through a scoped query.
///
/// # Errors
/// `ChatNotFound` or database errors.
pub async fn load_chat(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid) -> Result<chats::Model, DomainError> {
    chats::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)).add(chats::Column::DeletedAt.is_null()))
        .one(runner)
        .await?
        .ok_or(DomainError::ChatNotFound)
}

/// Counts non-deleted messages of a chat.
///
/// # Errors
/// Database errors.
pub async fn message_count(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    let n = messages::Entity::find()
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .filter(Condition::all().add(messages::Column::ChatId.eq(chat_id)).add(messages::Column::DeletedAt.is_null()))
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Bumps `chats.updated_at`.
///
/// # Errors
/// Database errors.
pub async fn touch_chat(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid, at: OffsetDateTime) -> Result<(), DomainError> {
    chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(at))
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

impl App {
    /// Resolves the model for a new chat.
    async fn resolve_new_chat_model(&self, user_id: Uuid, requested: Option<&str>) -> Result<String, DomainError> {
        let snap = self.policy.current_snapshot(user_id).await?;
        match requested {
            Some(m) => snap
                .find_enabled(m)
                .map(|e| e.id.clone())
                .ok_or(DomainError::InvalidModel),
            None => snap.default_model().map(|e| e.id.clone()).ok_or(DomainError::InvalidModel),
        }
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// Validation, authorization, model or database errors.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.map(|t| validate_title(&t)).transpose()?;
        let scope = chat_scope(&self.enforcer, ctx, actions::CREATE, None).await?;
        let model = self.resolve_new_chat_model(ctx.subject_id(), model.as_deref()).await?;
        let at = now();
        let am = chats::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            tenant_id: ActiveValue::Set(ctx.subject_tenant_id()),
            user_id: ActiveValue::Set(ctx.subject_id()),
            model: ActiveValue::Set(model),
            title: ActiveValue::Set(title),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(at),
            updated_at: ActiveValue::Set(at),
            deleted_at: ActiveValue::Set(None),
        };
        let conn = self.db.conn()?;
        let chat = toolkit_db::secure::secure_insert::<chats::Entity>(am, &scope, &conn)
            .await
            .map_err(|e| match e {
                toolkit_db::secure::ScopeError::Denied(_) => DomainError::AuthzDenied,
                other => other.into(),
            })?;
        Ok(ChatView { chat, message_count: 0 })
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// Authorization, not found or database errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::READ, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let n = message_count(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView { chat, message_count: n })
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// Authorization, OData or database errors.
    pub async fn list_chats(&self, ctx: &SecurityContext, query: &ODataQuery) -> Result<Page<ChatView>, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::LIST, None).await?;
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![OrderKey { field: "updated_at".into(), dir: SortDir::Desc }]);
        }
        let conn = self.db.conn()?;
        let base = chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()));
        let page = paginate_odata::<ChatField, ChatMapper, _, _, _, _>(base, &conn, &q, ("id", SortDir::Desc), PAGE_LIMITS, |m| m)
            .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for chat in page.items {
            let n = message_count(&conn, chat.tenant_id, chat.id).await?;
            items.push(ChatView { chat, message_count: n });
        }
        Ok(Page { items, page_info: page.page_info })
    }

    /// `PATCH /v1/chats/{id}`.
    ///
    /// # Errors
    /// Validation, authorization, not found or database errors.
    pub async fn update_chat_title(&self, ctx: &SecurityContext, chat_id: Uuid, title: &str) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = chat_scope(&self.enforcer, ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let at = now();
        chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(at))
            .filter(Condition::all().add(chats::Column::Id.eq(chat.id)).add(chats::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let n = message_count(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView { chat, message_count: n })
    }

    /// `DELETE /v1/chats/{id}`: soft delete, mark attachments for cleanup and
    /// enqueue chat cleanup in one transaction.
    ///
    /// # Errors
    /// Authorization, not found, outbox or database errors.
    pub async fn delete_chat(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::DELETE, Some(chat_id)).await?;
        let chat = {
            let conn = self.db.conn()?;
            load_chat(&conn, &scope, chat_id).await?
        };
        let app = Arc::clone(self);
        let wakes = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = now();
                    let rows = chats::Entity::update_many()
                        .col_expr(chats::Column::DeletedAt, Expr::value(Some(at)))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(at))
                        .filter(Condition::all().add(chats::Column::Id.eq(chat.id)).add(chats::Column::DeletedAt.is_null()))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Err(DomainError::ChatNotFound);
                    }
                    attachments::Entity::update_many()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(at)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat.id))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&tenant_scope(chat.tenant_id))
                        .exec(tx)
                        .await?;
                    let wake = app
                        .outbox
                        .chat_cleanup(
                            tx,
                            &ChatCleanupPayload {
                                tenant_id: chat.tenant_id,
                                chat_id: chat.id,
                                system_request_id: Uuid::new_v4(),
                                reason: "chat_soft_delete".into(),
                                chat_deleted_at: at,
                            },
                        )
                        .await?;
                    Ok(vec![wake])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }

    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// Authorization, not found, OData or database errors.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::LIST_MESSAGES, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![OrderKey { field: "created_at".into(), dir: SortDir::Asc }]);
        }
        let base = messages::Entity::find()
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat.id)).add(messages::Column::DeletedAt.is_null()));
        let page = paginate_odata::<MessageField, MessageMapper, _, _, _, _>(base, &conn, &q, ("id", SortDir::Asc), PAGE_LIMITS, |m| m)
            .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let atts = message_attachments_for(&conn, chat.tenant_id, chat.id, &ids).await?;
        let reactions = reactions_for(&conn, chat.tenant_id, ctx.subject_id(), &ids).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let request_id = m
                .request_id
                .ok_or_else(|| DomainError::internal(format!("message {} has no request_id", m.id)))?;
            items.push(MessageView {
                request_id,
                attachments: atts.get(&m.id).cloned().unwrap_or_default(),
                my_reaction: if m.role == "assistant" { reactions.get(&m.id).cloned() } else { None },
                message: m,
            });
        }
        Ok(Page { items, page_info: page.page_info })
    }

    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    ///
    /// # Errors
    /// Validation, authorization, not found, precondition or database errors.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<message_reactions::Model, DomainError> {
        if reaction != "like" && reaction != "dislike" {
            return Err(DomainError::InvalidReaction);
        }
        let scope = chat_scope(&self.enforcer, ctx, actions::SET_REACTION, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let msg = self.reaction_target(&conn, &chat, message_id).await?;
        let rscope = owner_scope(chat.tenant_id, ctx.subject_id());
        let at = now();
        let am = message_reactions::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            message_id: ActiveValue::Set(msg.id),
            user_id: ActiveValue::Set(ctx.subject_id()),
            tenant_id: ActiveValue::Set(chat.tenant_id),
            reaction: ActiveValue::Set(reaction.to_owned()),
            created_at: ActiveValue::Set(at),
        };
        message_reactions::Entity::insert(am)
            .secure()
            .scope_unchecked(&rscope)?
            .on_conflict_raw(
                OnConflict::columns([message_reactions::Column::MessageId, message_reactions::Column::UserId])
                    .update_columns([message_reactions::Column::Reaction, message_reactions::Column::CreatedAt])
                    .to_owned(),
            )
            .exec(&conn)
            .await?;
        message_reactions::Entity::find()
            .secure()
            .scope_with(&rscope)
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("reaction not found after upsert"))
    }

    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (idempotent).
    ///
    /// # Errors
    /// Authorization, not found, precondition or database errors.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, message_id: Uuid) -> Result<(), DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::DELETE_REACTION, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let msg = self.reaction_target(&conn, &chat, message_id).await?;
        use toolkit_db::secure::SecureDeleteExt;
        message_reactions::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&owner_scope(chat.tenant_id, ctx.subject_id()))
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn reaction_target(&self, runner: &impl DBRunner, chat: &chats::Model, message_id: Uuid) -> Result<messages::Model, DomainError> {
        let msg = messages::Entity::find()
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .filter(
                Condition::all()
                    .add(messages::Column::Id.eq(message_id))
                    .add(messages::Column::ChatId.eq(chat.id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .one(runner)
            .await?
            .ok_or(DomainError::MessageNotFound)?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTargetNotAssistant);
        }
        Ok(msg)
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// Authorization, not found or database errors.
    pub async fn get_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<chat_turns::Model, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::READ_TURN, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        chat_turns::Entity::find()
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .filter(
                Condition::all()
                    .add(chat_turns::Column::ChatId.eq(chat.id))
                    .add(chat_turns::Column::RequestId.eq(request_id))
                    .add(chat_turns::Column::DeletedAt.is_null()),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::TurnNotFound)
    }

    /// `GET /v1/models`.
    ///
    /// # Errors
    /// Authorization or policy errors.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        model_permission(&self.enforcer, ctx, actions::LIST).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snap.model_catalog.into_iter().filter(|m| m.enabled).collect())
    }

    /// `GET /v1/models/{id}`.
    ///
    /// # Errors
    /// Authorization, not found or policy errors.
    pub async fn get_model(&self, ctx: &SecurityContext, model_id: &str) -> Result<ModelCatalogEntry, DomainError> {
        model_permission(&self.enforcer, ctx, actions::READ).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        snap.find_enabled(model_id).cloned().ok_or(DomainError::ModelNotFound)
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// Authorization, policy or database errors.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> Result<Vec<PeriodStatus>, DomainError> {
        let scope = quota_scope(&self.enforcer, ctx).await?;
        self.quota_status_for(&scope, ctx.subject_tenant_id(), ctx.subject_id()).await
    }

    /// Quota status for a user (also used for `done.quota_warnings`).
    ///
    /// # Errors
    /// Policy or database errors.
    pub async fn quota_status_for(&self, scope: &AccessScope, tenant_id: Uuid, user_id: Uuid) -> Result<Vec<PeriodStatus>, DomainError> {
        let client = self.policy.client().await?;
        let version = client
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?
            .policy_version;
        let limits = self.policy.user_limits(user_id, version).await?;
        let at = now();
        let conn = self.db.conn()?;
        let usage = quota::load_usage(&conn, scope, tenant_id, user_id, PeriodStarts::at(at)).await?;
        Ok(quota::status(&usage, &limits, self.cfg.quota.warning_threshold_pct, at))
    }
}

/// Attachment summaries of messages (non-deleted attachments only).
///
/// # Errors
/// Database errors.
pub async fn message_attachments_for(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummary>>, DomainError> {
    let mut out: HashMap<Uuid, Vec<AttachmentSummary>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let scope = tenant_scope(tenant_id);
    let links = message_attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .order_by(message_attachments::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts: HashMap<Uuid, attachments::Model> = attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(attachments::Column::Id.is_in(att_ids))
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .all(runner)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    for l in links {
        if let Some(a) = atts.get(&l.attachment_id) {
            out.entry(l.message_id).or_default().push(summary_of(a));
        }
    }
    Ok(out)
}

/// Builds the attachment summary (thumbnail only for ready images).
#[must_use]
pub fn summary_of(a: &attachments::Model) -> AttachmentSummary {
    AttachmentSummary {
        attachment_id: a.id,
        kind: a.attachment_kind.clone(),
        filename: a.filename.clone(),
        status: a.status.clone(),
        thumbnail: thumbnail_of(a),
    }
}

/// Thumbnail of a ready image attachment.
#[must_use]
pub fn thumbnail_of(a: &attachments::Model) -> Option<Thumbnail> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    Some(Thumbnail {
        width: a.img_thumbnail_width?,
        height: a.img_thumbnail_height?,
        data: a.img_thumbnail.clone()?,
    })
}

async fn reactions_for(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(message_reactions::Entity::find()
        .secure()
        .scope_with(&owner_scope(tenant_id, user_id))
        .filter(
            Condition::all()
                .add(message_reactions::Column::UserId.eq(user_id))
                .add(message_reactions::Column::MessageId.is_in(message_ids.to_vec())),
        )
        .all(runner)
        .await?
        .into_iter()
        .map(|r| (r.message_id, r.reaction))
        .collect())
}
