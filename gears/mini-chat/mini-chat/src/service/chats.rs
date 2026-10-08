//! Chat CRUD.

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Set};
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::outbox::{ChatCleanupEvent, EnqueueError};
use crate::api::rest::odata::ChatQueryFieldsFilterField;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entity::{attachment, chat};
use crate::infra::odata::ChatODataMapper;
use crate::infra::repo::{self, now_utc};

/// Chat with its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: u64,
}

/// Validate a title: trimmed length 1..=255.
///
/// # Errors
/// 400 `INVALID_TITLE`.
pub fn validate_title(title: &str) -> DomainResult<String> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::invalid(
            Res::Chat,
            "title",
            "INVALID_TITLE",
            "title must be 1-255 characters after trimming",
        ));
    }
    Ok(t.to_owned())
}

impl AppState {
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> DomainResult<ChatView> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scopes = authz::chat_scopes(&self.enforcer, ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => snapshot
                .find_enabled(&m)
                .map(|e| e.id.clone())
                .ok_or_else(|| {
                    DomainError::invalid_model(format!("model '{m}' is not available"))
                })?,
            None => snapshot
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::invalid_model("no enabled model in the catalog"))?,
        };
        let now = now_utc();
        let am = chat::ActiveModel {
            id: Set(Uuid::now_v7()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model_id),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        let conn = self.conn()?;
        let created = secure_insert::<chat::Entity>(am, &scopes.owner, &conn).await?;
        Ok(ChatView {
            chat: created,
            message_count: 0,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<ChatView> {
        let scopes = authz::chat_scopes(&self.enforcer, ctx, actions::READ, Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let message_count = repo::count_messages(&conn, &scopes.tenant, chat_id).await?;
        Ok(ChatView {
            chat,
            message_count,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> DomainResult<Page<ChatView>> {
        let scopes = authz::chat_scopes(&self.enforcer, ctx, actions::LIST, None).await?;
        let conn = self.conn()?;
        let mut q = query.clone();
        if q.order.0.is_empty() && q.cursor.is_none() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let select = chat::Entity::find()
            .secure()
            .scope_with(&scopes.owner)
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()));
        let page = paginate_odata::<ChatQueryFieldsFilterField, ChatODataMapper, _, _, _, _>(
            select,
            &conn,
            &q,
            ("id", SortDir::Desc),
            LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for c in page.items {
            let n = repo::count_messages(&conn, &scopes.tenant, c.id).await?;
            items.push(ChatView {
                chat: c,
                message_count: n,
            });
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn update_chat_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> DomainResult<ChatView> {
        let title = validate_title(title)?;
        let scopes =
            authz::chat_scopes(&self.enforcer, ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.conn()?;
        let now = now_utc();
        let res = chat::Entity::update_many()
            .secure()
            .scope_with(&scopes.owner)
            .col_expr(chat::Column::Title, Expr::value(Some(title)))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(chat_id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .exec(&conn)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::not_found(Res::Chat, chat_id));
        }
        let chat = repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let message_count = repo::count_messages(&conn, &scopes.tenant, chat_id).await?;
        Ok(ChatView {
            chat,
            message_count,
        })
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn delete_chat(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> DomainResult<()> {
        let scopes =
            authz::chat_scopes(&self.enforcer, ctx, actions::DELETE, Some(chat_id)).await?;
        let outbox = self.outbox.get().await?;
        let state = Arc::clone(self);
        let wake = self
            .write_tx(move |tx| {
                let scopes = scopes.clone();
                let outbox = Arc::clone(&outbox);
                let state = Arc::clone(&state);
                Box::pin(async move {
                    let chat = repo::find_chat(tx, &scopes.owner, chat_id)
                        .await?
                        .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
                    let now = now_utc();
                    let res = chat::Entity::update_many()
                        .secure()
                        .scope_with(&scopes.owner)
                        .col_expr(chat::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat::Column::Id.eq(chat_id))
                                .add(chat::Column::DeletedAt.is_null()),
                        )
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::not_found(Res::Chat, chat_id));
                    }
                    attachment::Entity::update_many()
                        .secure()
                        .scope_with(&scopes.tenant)
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending")),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(chat_id))
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .exec(tx)
                        .await?;
                    let ev = ChatCleanupEvent {
                        tenant_id: chat.tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: now,
                    };
                    state
                        .enqueue_chat_cleanup(&outbox, tx, &ev)
                        .await
                        .map_err(|e| match e {
                            EnqueueError::TooLarge(m) => DomainError::InvalidFormat {
                                res: Res::Chat,
                                message: m,
                            },
                            other @ EnqueueError::Other(_) => DomainError::from(other),
                        })
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_validation() {
        assert_eq!(validate_title("  hi  ").unwrap(), "hi");
        assert!(validate_title("   ").is_err());
        assert!(validate_title(&"a".repeat(256)).is_err());
        assert!(validate_title(&"a".repeat(255)).is_ok());
    }
}
