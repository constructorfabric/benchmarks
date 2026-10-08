//! Chat CRUD (DESIGN §3.3 Create/List/Get/Update/Delete chat).

use sea_orm::EntityTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use serde::Serialize;
use time::OffsetDateTime;
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::odata_fields::{ChatField, ChatMapper};
use crate::domain::state::AppState;
use crate::infra::db::entities::{attachments, chats};
use crate::infra::db::repo;
use crate::infra::outbox::{Queue, Wakes};

pub const LIST_LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };

#[derive(Debug, Clone)]
pub struct ChatView {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatView {
    fn from_model(m: &chats::Model, message_count: i64) -> Self {
        Self {
            id: m.id,
            model: m.model.clone(),
            title: m.title.clone(),
            is_temporary: m.is_temporary,
            message_count,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

/// Chat-cleanup outbox payload.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ChatCleanupPayload {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Trim and validate a title (1..=255 characters after trim).
///
/// # Errors
/// 400 `INVALID_TITLE`.
pub fn validate_title(raw: &str) -> DomainResult<String> {
    let t = raw.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::field(
            Res::Chat,
            "title",
            "INVALID_TITLE",
            "title must be 1-255 characters after trimming",
        ));
    }
    Ok(t.to_owned())
}

impl AppState {
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> DomainResult<ChatView> {
        let title = match title {
            Some(t) => Some(validate_title(&t)?),
            None => None,
        };
        let scopes = self.chat_scope(ctx, "create", None).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        let entry = match model.as_deref() {
            Some(m) => snap.find_enabled_model(m),
            None => snap.default_model(),
        }
        .ok_or_else(DomainError::invalid_model)?;
        let now = repo::now();
        let am = chats::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(entry.id.clone()),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let m = repo::insert_chat(&conn, &scopes.chat, am).await?;
        Ok(ChatView::from_model(&m, 0))
    }

    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<ChatView> {
        let scopes = self.chat_scope(ctx, "read", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let m = repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let n = repo::count_messages(&conn, &scopes.tenant, chat_id).await?;
        Ok(ChatView::from_model(&m, n))
    }

    pub async fn list_chats(&self, ctx: &SecurityContext, mut q: ODataQuery) -> DomainResult<Page<ChatView>> {
        let scopes = self.chat_scope(ctx, "list", None).await?;
        if q.cursor.is_none() && q.order.is_empty() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let conn = self.db.conn()?;
        let select = chats::Entity::find()
            .secure()
            .scope_with(&scopes.chat)
            .filter(Condition::all().add(chats::Column::DeletedAt.is_null()));
        let page = paginate_odata::<ChatField, ChatMapper, chats::Entity, chats::Model, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Desc),
            LIST_LIMITS,
            |m| m,
        )
        .await
        .map_err(DomainError::from)?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in &page.items {
            let n = repo::count_messages(&conn, &scopes.tenant, m.id).await?;
            items.push(ChatView::from_model(m, n));
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    pub async fn update_chat_title(&self, ctx: &SecurityContext, chat_id: Uuid, title: &str) -> DomainResult<ChatView> {
        let title = validate_title(title)?;
        let scopes = self.chat_scope(ctx, "update", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let now = repo::now();
        chats::Entity::update_many()
            .secure()
            .col_expr(chats::Column::Title, Expr::value(Some(title)))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chats::Column::Id.eq(chat_id))
                    .add(chats::Column::DeletedAt.is_null()),
            )
            .scope_with(&scopes.chat)
            .exec(&conn)
            .await?;
        let m = repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let n = repo::count_messages(&conn, &scopes.tenant, chat_id).await?;
        Ok(ChatView::from_model(&m, n))
    }

    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<()> {
        let scopes = self.chat_scope(ctx, "delete", Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let outbox = self.outbox.clone();
        let wakes = self
            .write_tx(move |tx| {
                let scopes = scopes.clone();
                let outbox = outbox.clone();
                Box::pin(async move {
                    let now = repo::now();
                    let res = chats::Entity::update_many()
                        .secure()
                        .col_expr(chats::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chats::Column::Id.eq(chat_id))
                                .add(chats::Column::DeletedAt.is_null()),
                        )
                        .scope_with(&scopes.chat)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::chat_not_found(chat_id));
                    }
                    attachments::Entity::update_many()
                        .secure()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .scope_with(&scopes.tenant)
                        .exec(tx)
                        .await?;
                    let payload = ChatCleanupPayload {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.enqueue(tx, Queue::ChatCleanup, chat_id, &payload).await?);
                    Ok(wakes)
                })
            })
            .await?;
        wakes.fire();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_title;

    #[test]
    fn title_rules() {
        assert_eq!(validate_title("  hi  ").unwrap(), "hi");
        assert!(validate_title("   ").is_err());
        assert!(validate_title("").is_err());
        assert!(validate_title(&"a".repeat(255)).is_ok());
        assert!(validate_title(&"a".repeat(256)).is_err());
    }
}
