//! Chat CRUD (DESIGN §3.3).

use std::collections::HashMap;

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataOrderBy, ODataQuery, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::db::WriteTransaction as _;
use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::service::{Svc, child_scope};
use crate::infra::db::entities::{attachments, chats, messages};
use crate::infra::db::now;
use crate::infra::db::odata::{ChatField, ChatMapper};
use crate::infra::outbox::ChatCleanupEvent;

/// Page size limits of list endpoints.
pub const LIMIT_CFG: LimitCfg = LimitCfg { default: 20, max: 100 };

/// Chat plus its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    /// Row.
    pub chat: chats::Model,
    /// Non-deleted messages.
    pub message_count: i64,
}

/// Validates and trims a title.
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let t = raw.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Counts non-deleted messages per chat.
///
/// # Errors
/// Database errors.
pub async fn message_counts(
    runner: &impl DBRunner,
    tenant_ids: &[Uuid],
    chat_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = toolkit_security::AccessScope::for_tenants(tenant_ids.to_vec());
    let rows: Vec<ChatCount> = messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.is_in(chat_ids.to_vec()))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .project_all(runner, |q| {
            q.select_only()
                .column(messages::Column::ChatId)
                .column_as(Expr::col(messages::Column::Id).count(), "cnt")
                .group_by(messages::Column::ChatId)
                .into_model::<ChatCount>()
        })
        .await?;
    Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
}

impl Svc {
    async fn chat_view(&self, chat: chats::Model) -> Result<ChatView, DomainError> {
        let conn = self.db.conn()?;
        let counts = message_counts(&conn, &[chat.tenant_id], &[chat.id]).await?;
        let message_count = counts.get(&chat.id).copied().unwrap_or(0);
        Ok(ChatView { chat, message_count })
    }

    /// `POST /chats`.
    ///
    /// # Errors
    /// `InvalidTitle`, `InvalidModel`, PDP errors, internal.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot.enabled_model(&m).map(|e| e.id.clone()).ok_or(DomainError::InvalidModel)?,
            None => snapshot.default_model().map(|e| e.id.clone()).ok_or(DomainError::InvalidModel)?,
        };
        let ts = now();
        let am = chats::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model_id),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(ts),
            updated_at: Set(ts),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let chat = secure_insert::<chats::Entity>(am, &scope, &conn).await?;
        Ok(ChatView { chat, message_count: 0 })
    }

    /// `GET /chats/{id}`.
    ///
    /// # Errors
    /// 404 / PDP errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ, chat_id).await?;
        self.chat_view(chat).await
    }

    /// `GET /chats`.
    ///
    /// # Errors
    /// `OData` errors (400), PDP errors.
    pub async fn list_chats(&self, ctx: &SecurityContext, query: &ODataQuery) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        let mut q = query.clone();
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy::empty().ensure_tiebreaker("updated_at", SortDir::Desc);
        }
        let conn = self.db.conn()?;
        let select = chats::Entity::find()
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scope);
        let q = crate::infra::db::odata::sqlite_safe_query(&q, self.db.db().backend(), &["updated_at"]);
        let page = paginate_odata::<ChatField, ChatMapper, chats::Entity, chats::Model, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Desc),
            LIMIT_CFG,
            |m| m,
        )
        .await?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts = message_counts(&conn, &[ctx.subject_tenant_id()], &ids).await?;
        Ok(page.map_items(|chat| {
            let message_count = counts.get(&chat.id).copied().unwrap_or(0);
            ChatView { chat, message_count }
        }))
    }

    /// `PATCH /chats/{id}`.
    ///
    /// # Errors
    /// `InvalidTitle`, 404, PDP errors.
    pub async fn rename_chat(&self, ctx: &SecurityContext, chat_id: Uuid, title: &str) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let (scope, _) = self.authorized_chat(ctx, actions::UPDATE, chat_id).await?;
        let conn = self.db.conn()?;
        let res = chats::Entity::update_many()
            .secure()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now()))
            .filter(
                Condition::all()
                    .add(chats::Column::Id.eq(chat_id))
                    .add(chats::Column::DeletedAt.is_null()),
            )
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::ChatNotFound);
        }
        let chat = crate::domain::service::load_chat(&conn, &scope, chat_id).await?;
        self.chat_view(chat).await
    }

    /// `DELETE /chats/{id}`: soft delete, mark attachments for cleanup, enqueue chat cleanup.
    ///
    /// # Errors
    /// 404, PDP errors, payload too large (400), internal.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let (scope, chat) = self.authorized_chat(ctx, actions::DELETE, chat_id).await?;
        let outbox = self.outbox.clone();
        let tenant_scope = child_scope(&chat);
        let tenant_id = chat.tenant_id;
        let wake = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let res = chats::Entity::update_many()
                        .secure()
                        .col_expr(chats::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(chats::Column::Id.eq(chat_id))
                                .add(chats::Column::DeletedAt.is_null()),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::ChatNotFound);
                    }
                    attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .scope_with(&tenant_scope)
                        .exec(tx)
                        .await?;
                    let event = ChatCleanupEvent {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: ts,
                    };
                    outbox.chat_cleanup(tx, &event).await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}
