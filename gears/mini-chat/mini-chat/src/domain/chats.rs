//! Chat CRUD (DESIGN §3.3).

use std::collections::HashMap;

use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Resource, retry_contention};
use crate::domain::policy;
use crate::domain::time::now;
use crate::infra::db::entities::chat;
use crate::infra::db::odata::{ChatFilterField, ChatMapper};
use crate::infra::db::repo;
use crate::infra::outbox::{ChatCleanupPayload, Wakes};

/// A chat with its message count.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: chat::Model,
    pub message_count: i64,
}

/// Validates and trims a title: 1-255 characters after trimming.
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(raw: &str) -> DomainResult<String> {
    let t = raw.trim();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(DomainError::InvalidTitle);
    }
    Ok(t.to_owned())
}

impl AppServices {
    /// Creates a chat (title validated before authorization and model lookup).
    ///
    /// # Errors
    /// `InvalidTitle`, `InvalidModel`, authorization errors.
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
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model_id = match model {
            Some(m) => {
                let entry = snapshot.find(&m).filter(|e| e.enabled).ok_or_else(|| {
                    DomainError::InvalidModel(format!("model '{m}' is not available"))
                })?;
                entry.id.clone()
            }
            None => policy::default_model(&snapshot)
                .ok_or_else(|| {
                    DomainError::InvalidModel("no enabled model is available".to_owned())
                })?
                .id
                .clone(),
        };
        let ts = now();
        let m = chat::Model {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model: model_id,
            title,
            is_temporary: false,
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
        };
        let conn = self.db.conn()?;
        let created = repo::insert_chat(&conn, &scope, m).await?;
        Ok(ChatView {
            chat: created,
            message_count: 0,
        })
    }

    /// Loads a chat under the PEP scope of `action`.
    ///
    /// # Errors
    /// `NotFound(Chat)` and authorization errors.
    pub async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> DomainResult<chat::Model> {
        let scope = authz::chat_scope(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::find_chat(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound(Resource::Chat))
    }

    async fn with_count(&self, chat: chat::Model) -> DomainResult<ChatView> {
        let conn = self.db.conn()?;
        let counts = repo::message_counts(&conn, chat.tenant_id, &[chat.id]).await?;
        let message_count = counts.get(&chat.id).copied().unwrap_or(0);
        Ok(ChatView {
            chat,
            message_count,
        })
    }

    /// # Errors
    /// `NotFound(Chat)` and authorization errors.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<ChatView> {
        let chat = self.authorized_chat(ctx, actions::READ, chat_id).await?;
        self.with_count(chat).await
    }

    /// Lists the caller's chats: default order `updated_at desc`, `id` tiebreaker.
    ///
    /// # Errors
    /// `OData` errors are returned as `toolkit_odata::Error` via [`ListError`].
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatView>, ListError> {
        use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
        use toolkit_db::secure::SecureEntityExt;
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::LIST, None).await?;
        let conn = self.db.conn().map_err(ListError::Domain)?;
        let mut query = query.clone();
        if query.order.0.is_empty() && query.cursor.is_none() {
            query.order = toolkit_odata::ODataOrderBy(vec![toolkit_odata::OrderKey {
                field: "updated_at".to_owned(),
                dir: SortDir::Desc,
            }]);
        }
        let select = chat::Entity::find()
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scope);
        let page = toolkit_db::odata::paginate_odata::<
            ChatFilterField,
            ChatMapper,
            chat::Entity,
            chat::Model,
            _,
            _,
        >(
            select,
            &conn,
            &query,
            ("id", SortDir::Desc),
            toolkit_db::odata::LimitCfg {
                default: 20,
                max: 100,
            },
            |m| m,
        )
        .await
        .map_err(ListError::OData)?;
        let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
        let counts: HashMap<Uuid, i64> = repo::message_counts(&conn, ctx.subject_tenant_id(), &ids)
            .await
            .map_err(ListError::Domain)?;
        Ok(page.map_items(|c| {
            let message_count = counts.get(&c.id).copied().unwrap_or(0);
            ChatView {
                chat: c,
                message_count,
            }
        }))
    }

    /// Renames a chat.
    ///
    /// # Errors
    /// `InvalidTitle`, `NotFound(Chat)`, authorization errors.
    pub async fn update_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> DomainResult<ChatView> {
        let title = validate_title(title)?;
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let n = repo::update_chat_title(&conn, &scope, chat_id, title, now()).await?;
        if n == 0 {
            return Err(DomainError::NotFound(Resource::Chat));
        }
        let chat = repo::find_chat(&conn, &scope, chat_id)
            .await?
            .ok_or(DomainError::NotFound(Resource::Chat))?;
        self.with_count(chat).await
    }

    /// Soft-deletes a chat, marks its attachments for cleanup and enqueues the chat cleanup.
    ///
    /// # Errors
    /// `NotFound(Chat)`, `PayloadTooLarge` (400), authorization errors.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<()> {
        let scope = authz::chat_scope(&self.enforcer, ctx, actions::DELETE, Some(chat_id)).await?;
        let outbox = std::sync::Arc::clone(&self.outbox);
        let tenant_id = ctx.subject_tenant_id();
        let wakes = retry_contention(|| {
            let outbox = std::sync::Arc::clone(&outbox);
            let scope = scope.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let n = repo::soft_delete_chat(tx, &scope, chat_id, ts).await?;
                    if n == 0 {
                        return Err(DomainError::NotFound(Resource::Chat));
                    }
                    repo::mark_chat_attachments_pending(tx, tenant_id, chat_id, ts).await?;
                    let payload = ChatCleanupPayload {
                        tenant_id,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".to_owned(),
                        chat_deleted_at: ts,
                    };
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.chat_cleanup(tx, &payload).await?);
                    Ok(wakes)
                })
            })
        })
        .await?;
        wakes.fire();
        Ok(())
    }
}

/// Error of list operations: `OData` errors keep their canonical mapping.
#[derive(Debug)]
pub enum ListError {
    Domain(DomainError),
    OData(toolkit_odata::Error),
}

impl From<DomainError> for ListError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}
