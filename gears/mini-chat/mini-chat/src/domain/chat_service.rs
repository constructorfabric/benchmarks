//! Chat CRUD service (DESIGN section 3.3 "Create Chat", "List Chats", "Get
//! Chat", "Update Chat Title", "Delete Chat"; section 3.8; "Cleanup on Chat
//! Deletion"; ADR-0009).
//!
//! - **Create** — the title is validated before the authorization check and
//!   the model lookup; the model is the requested enabled catalog model or the
//!   default model, and stays locked for the chat lifetime
//!   (`constraint-model-locked-per-chat`). Tenant and owner come from the
//!   security context only.
//! - **Read** — every chat read goes through the scoped chat query; a chat
//!   that is unknown, deleted or another user's is the same
//!   `404 not_found` (chat).
//! - **Update** — only the title changes; `updated_at` is bumped.
//! - **Delete** — soft delete in one transaction together with the
//!   attachment cleanup marks and the chat-cleanup outbox message; children
//!   are not modified; a second delete is 404.
//!
//! `message_count` counts the chat's non-deleted messages; lists compute it
//! for the whole page in one query.

use std::collections::HashMap;
use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Action, AuthzService};
use crate::domain::error::{DomainError, InvalidArgumentReason, ResourceKind};
use crate::domain::models_service::ModelsService;
use crate::domain::ports::{ChatDeletion, ChatRecord, ChatRepository, NewChat, storage_now};

/// Maximum chat title length in characters, after trimming.
pub const MAX_TITLE_CHARS: usize = 255;

/// `ChatDetail`: the chat as returned by every chat endpoint (create, get,
/// list items, update). `title` is `None` for an untitled chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub is_temporary: bool,
    /// Number of non-deleted messages of the chat.
    pub message_count: u64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatDetail {
    fn from_record(record: ChatRecord, message_count: u64) -> Self {
        Self {
            id: record.id,
            model: record.model,
            title: record.title,
            is_temporary: record.is_temporary,
            message_count,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// Trims `raw` and checks the 1..=255 character bound.
///
/// # Errors
///
/// `400 invalid_argument` (`title` / `INVALID_TITLE`) when the trimmed title
/// is empty or longer than [`MAX_TITLE_CHARS`] characters.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    let chars = trimmed.chars().count();
    if chars == 0 || chars > MAX_TITLE_CHARS {
        return Err(DomainError::invalid_argument(
            InvalidArgumentReason::InvalidTitle,
        ));
    }
    Ok(trimmed.to_owned())
}

/// Chat CRUD domain service.
pub struct ChatService {
    chats: Arc<dyn ChatRepository>,
    models: Arc<ModelsService>,
    authz: Arc<AuthzService>,
}

impl std::fmt::Debug for ChatService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatService")
            .field("authz", &self.authz)
            .finish_non_exhaustive()
    }
}

impl ChatService {
    /// Builds the service over the chat repository, the model resolution of
    /// the Models API service and the gear PEP.
    #[must_use]
    pub fn new(
        chats: Arc<dyn ChatRepository>,
        models: Arc<ModelsService>,
        authz: Arc<AuthzService>,
    ) -> Self {
        Self {
            chats,
            models,
            authz,
        }
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    ///
    /// `INVALID_TITLE` (checked first), PEP errors of `create`,
    /// `INVALID_MODEL` (unknown or disabled model, or no enabled model when
    /// none is requested), [`DomainError::Internal`] on plugin or storage
    /// failure.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatDetail, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.authorize_chat(ctx, Action::Create, None).await?;
        let resolved = self
            .models
            .resolve_for_new_chat(ctx, model.as_deref())
            .await?;
        let record = self
            .chats
            .insert(
                &scope,
                NewChat {
                    id: Uuid::new_v4(),
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    model: resolved.model.id,
                    title,
                    now: storage_now(),
                },
            )
            .await?;
        Ok(ChatDetail::from_record(record, 0))
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    ///
    /// PEP errors of `read`; `404` (chat) when the chat is unknown, deleted
    /// or not the caller's.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        let scope = self
            .authz
            .authorize_chat(ctx, Action::Read, Some(chat_id))
            .await?;
        let record = self
            .chats
            .find_active(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        self.detail(&scope, record).await
    }

    /// `GET /v1/chats`: one page of the caller's non-deleted chats.
    ///
    /// # Errors
    ///
    /// PEP errors of `list`; [`DomainError::InvalidQuery`] for an invalid
    /// `OData` query (unknown field, malformed filter or cursor, `limit = 0`).
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        if query.limit == Some(0) {
            return Err(DomainError::InvalidQuery(
                toolkit_odata::Error::InvalidLimit,
            ));
        }
        let scope = self.authz.authorize_chat(ctx, Action::List, None).await?;
        let page = self.chats.list_active(&scope, query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|chat| chat.id).collect();
        let counts = if ids.is_empty() {
            HashMap::new()
        } else {
            self.chats.count_active_messages(&scope, &ids).await?
        };
        let items = page
            .items
            .into_iter()
            .map(|record| {
                let count = counts.get(&record.id).copied().unwrap_or(0);
                ChatDetail::from_record(record, count)
            })
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PATCH /v1/chats/{id}`: sets the title only.
    ///
    /// # Errors
    ///
    /// `INVALID_TITLE` (checked first), PEP errors of `update`, `404` (chat).
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        let title = validate_title(title)?;
        let scope = self
            .authz
            .authorize_chat(ctx, Action::Update, Some(chat_id))
            .await?;
        let record = self
            .chats
            .update_title(&scope, chat_id, &title, storage_now())
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        self.detail(&scope, record).await
    }

    /// `DELETE /v1/chats/{id}`: soft delete with cleanup scheduling.
    ///
    /// # Errors
    ///
    /// PEP errors of `delete`; `404` (chat) when the chat is unknown, already
    /// deleted or not the caller's; [`DomainError::InvalidFormat`] when the
    /// chat-cleanup payload exceeds the outbox size limit.
    pub async fn delete(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = self
            .authz
            .authorize_chat(ctx, Action::Delete, Some(chat_id))
            .await?;
        let deleted = self
            .chats
            .soft_delete(
                &scope,
                ChatDeletion {
                    chat_id,
                    deleted_at: storage_now(),
                    system_request_id: Uuid::new_v4(),
                },
            )
            .await?;
        if deleted {
            Ok(())
        } else {
            Err(chat_not_found(chat_id))
        }
    }

    async fn detail(
        &self,
        scope: &toolkit_security::AccessScope,
        record: ChatRecord,
    ) -> Result<ChatDetail, DomainError> {
        let counts = self
            .chats
            .count_active_messages(scope, &[record.id])
            .await?;
        let count = counts.get(&record.id).copied().unwrap_or(0);
        Ok(ChatDetail::from_record(record, count))
    }
}

/// `404 not_found` on the chat resource.
#[must_use]
pub fn chat_not_found(chat_id: Uuid) -> DomainError {
    DomainError::not_found(ResourceKind::Chat, chat_id.to_string())
}

#[cfg(test)]
#[path = "chat_service_tests.rs"]
pub(crate) mod tests;
