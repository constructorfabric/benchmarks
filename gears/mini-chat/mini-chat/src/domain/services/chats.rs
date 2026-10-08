//! Chat CRUD (DESIGN §3.3 "Create/List/Get/Update/Delete Chat").

use std::sync::Arc;

use mini_chat_sdk::PolicySnapshot;
use sea_orm::ActiveValue::Set;
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::infra::db::entities::chats;
use crate::infra::db::odata::{ChatMapper, ChatQueryFieldsFilterField};
use crate::infra::db::repo;
use crate::infra::outbox::payloads::ChatCleanupPayload;

/// Chat with its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chats::Model,
    pub message_count: i64,
}

/// Validates and trims a title (1–255 characters after trim).
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(title: &str) -> Result<String, DomainError> {
    let t = title.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

/// Default model: first enabled `is_default`, else first enabled.
#[must_use]
pub fn default_model(snapshot: &PolicySnapshot) -> Option<String> {
    let enabled = || snapshot.model_catalog.iter().filter(|m| m.enabled);
    enabled()
        .find(|m| m.is_default())
        .or_else(|| enabled().next())
        .map(|m| m.id.clone())
}

impl MiniChatService {
    async fn view(&self, chat: chats::Model) -> Result<ChatView, DomainError> {
        let conn = self.db.conn()?;
        let message_count = repo::messages::count_for_chat(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView { chat, message_count })
    }

    /// Creates a chat.
    ///
    /// # Errors
    /// `InvalidTitle`, `InvalidModel`, authorization errors.
    pub async fn create_chat(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = match model {
            Some(m) => {
                if !snapshot.model_catalog.iter().any(|e| e.id == m && e.enabled) {
                    return Err(DomainError::InvalidModel);
                }
                m
            }
            None => default_model(&snapshot).ok_or(DomainError::InvalidModel)?,
        };
        let now = clock::now();
        let model = chats::Model {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model,
            title,
            is_temporary: false,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let am = chats::ActiveModel {
            id: Set(model.id),
            tenant_id: Set(model.tenant_id),
            user_id: Set(model.user_id),
            model: Set(model.model.clone()),
            title: Set(model.title.clone()),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        repo::chats::insert(&conn, &scope, am).await?;
        Ok(ChatView {
            chat: model,
            message_count: 0,
        })
    }

    /// Gets a chat.
    ///
    /// # Errors
    /// 404 / authorization errors.
    pub async fn get_chat(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let (_, chat) = self.authorized_chat(ctx, actions::READ, chat_id).await?;
        self.view(chat).await
    }

    /// Lists chats (default order `updated_at desc, id desc`).
    ///
    /// # Errors
    /// `OData` errors (400) / authorization errors.
    pub async fn list_chats(self: &Arc<Self>, ctx: &SecurityContext, query: &ODataQuery) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        let mut q = query.clone();
        if q.order.is_empty() && q.cursor.is_none() {
            q.order = ODataOrderBy(vec![OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let conn = self.db.conn()?;
        let page = paginate_odata::<ChatQueryFieldsFilterField, ChatMapper, _, _, _, _>(
            repo::chats::list_select(&scope),
            &conn,
            &q,
            ("id", SortDir::Desc),
            LimitCfg { default: 20, max: 100 },
            |m| m,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for chat in page.items {
            items.push(self.view(chat).await?);
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// Renames a chat.
    ///
    /// # Errors
    /// `InvalidTitle`, 404, authorization errors.
    pub async fn update_chat(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = self.authz.chat_scope(ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let n = repo::chats::update_title(&conn, &scope, chat_id, &title, clock::now()).await?;
        if n == 0 {
            return Err(DomainError::ChatNotFound);
        }
        let chat = self.load_chat(&scope, chat_id).await?;
        self.view(chat).await
    }

    /// Soft-deletes a chat and enqueues provider cleanup.
    ///
    /// # Errors
    /// 404, authorization errors, outbox payload errors.
    pub async fn delete_chat(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::DELETE, Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let outbox = Arc::clone(&self.outbox);
        self.transact(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let n = repo::chats::soft_delete(tx, &scope, chat_id, now).await?;
                if n == 0 {
                    return Err(DomainError::ChatNotFound);
                }
                repo::attachments::mark_chat_cleanup_pending(tx, tenant_id, chat_id, now).await?;
                let wake = outbox
                    .chat_cleanup(
                        tx,
                        &ChatCleanupPayload {
                            tenant_id,
                            chat_id,
                            system_request_id: Uuid::new_v4(),
                            reason: "chat_soft_delete".to_owned(),
                            chat_deleted_at: now,
                        },
                    )
                    .await?;
                Ok(((), wake))
            })
        })
        .await
    }
}
