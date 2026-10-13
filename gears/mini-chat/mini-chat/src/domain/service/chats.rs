//! Chats service: title validation, chat model resolution, chat create, get,
//! list, rename and soft delete
//! (`cpt-cf-mini-chat-feature-chat-crud`, slices 1 to 4).

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{AuthzService, ChatAction};
use crate::domain::clock::now_micros;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};

/// Maximum title length in Unicode scalar values (after trim).
pub const TITLE_MAX_CHARS: usize = 255;

/// Default page size of the list endpoints.
pub const DEFAULT_PAGE_LIMIT: u64 = 20;

/// Maximum page size of the list endpoints (larger values are clamped).
pub const MAX_PAGE_LIMIT: u64 = 100;

/// Validates and trims a chat title
/// (`cpt-cf-mini-chat-algo-chat-crud-validate-title`).
///
/// # Errors
///
/// [`DomainError::InvalidTitle`] when the trimmed title has 0 or more than
/// [`TITLE_MAX_CHARS`] characters.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-trim-title
    let trimmed = raw.trim();
    let length = trimmed.chars().count();
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-trim-title

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-if-title-empty
    if length == 0 {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title-empty
        return Err(DomainError::InvalidTitle);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title-empty
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-if-title-empty

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-if-title-long
    if length > TITLE_MAX_CHARS {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title-long
        return Err(DomainError::InvalidTitle);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title-long
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-if-title-long

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title
    Ok(trimmed.to_owned())
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-validate-title:p1:inst-return-title
}

/// Resolves the model a new chat is created on
/// (`cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`, steps 3 to 6; the
/// catalog read of steps 1 and 2 is done by the caller).
///
/// # Errors
///
/// [`DomainError::InvalidModel`] when `requested` names no enabled catalog
/// entry, or when `requested` is `None` and no entry is enabled.
pub fn resolve_chat_model(
    requested: Option<&str>,
    catalog: &[ModelCatalogEntry],
) -> Result<String, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-model-given
    if let Some(requested) = requested {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-model-invalid
        let entry = catalog
            .iter()
            .find(|entry| entry.id == requested && entry.enabled);
        let Some(entry) = entry else {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-model-invalid
            return Err(DomainError::InvalidModel);
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-model-invalid
        };
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-model-invalid
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-given-model
        return Ok(entry.id.clone());
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-given-model
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-model-given

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-for-each-default-candidate
    for entry in catalog {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-default-entry
        if entry.enabled && entry.preference.as_ref().is_some_and(|p| p.is_default) {
            // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-default-entry
            return Ok(entry.id.clone());
            // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-default-entry
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-default-entry
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-for-each-default-candidate

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-any-enabled
    if let Some(entry) = catalog.iter().find(|entry| entry.enabled) {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-first-enabled
        return Ok(entry.id.clone());
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-first-enabled
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-if-any-enabled

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-no-enabled
    Err(DomainError::InvalidModel)
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-return-no-enabled
}

/// Projects one chat with its message count (never `tenant_id`, `user_id` or
/// `deleted_at`).
fn project_chat(chat: Chat, message_count: u64) -> ChatDetail {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-project-chat-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-chat-non-exposure
    ChatDetail {
        id: chat.id,
        model: chat.model,
        title: chat.title,
        is_temporary: false,
        message_count,
        created_at: chat.created_at,
        updated_at: chat.updated_at,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-chat-non-exposure
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-project-chat-fields
}

/// Projection of a chat with its `message_count`
/// (`cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    /// Chat id.
    pub id: Uuid,
    /// Model the chat was created on.
    pub model: String,
    /// Title, if any.
    pub title: Option<String>,
    /// Always `false` in P1.
    pub is_temporary: bool,
    /// Non-deleted messages of the chat, all roles.
    pub message_count: u64,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub updated_at: OffsetDateTime,
}

/// Chat create, get, list, rename and delete behind the PEP.
pub struct ChatsService {
    authz: Arc<AuthzService>,
    policy: Arc<dyn PolicyCatalog>,
    chats: Arc<dyn ChatRepository>,
}

impl ChatsService {
    /// Creates the service.
    #[must_use]
    pub fn new(
        authz: Arc<AuthzService>,
        policy: Arc<dyn PolicyCatalog>,
        chats: Arc<dyn ChatRepository>,
    ) -> Self {
        Self {
            authz,
            policy,
            chats,
        }
    }

    /// Reads the current catalog and resolves the model of a new chat
    /// (`cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`).
    async fn resolve_model(
        &self,
        ctx: &SecurityContext,
        requested: Option<&str>,
    ) -> Result<String, DomainError> {
        let user_id = ctx.subject_id();
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-resolve-policy-plugin
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-read-catalog
        // The plugin is resolved lazily by the gateway behind `PolicyCatalog`;
        // a missing plugin, a failing call and a timeout all surface as
        // `PolicyResolution` (500 `internal`), and nothing is cached.
        let version = self.policy.current_policy_version(user_id).await?;
        let snapshot = self
            .policy
            .policy_snapshot(user_id, version.policy_version)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-read-catalog
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model:p1:inst-resolve-policy-plugin
        resolve_chat_model(requested, &snapshot.model_catalog)
    }

    /// Projects `chats` with their message counts, one batched count query for
    /// the whole input (`cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`,
    /// the existing-chat branch).
    async fn project(
        &self,
        scope: &AccessScope,
        chats: Vec<Chat>,
    ) -> Result<Vec<ChatDetail>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-else-count-query
        let ids: Vec<Uuid> = chats.iter().map(|chat| chat.id).collect();
        let counts: HashMap<Uuid, u64> = self.chats.message_counts(scope, &ids).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-else-count-query

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-for-each-chat
        let details = chats
            .into_iter()
            .map(|chat| {
                // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-default-count
                let count = counts.get(&chat.id).copied().unwrap_or(0);
                // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-default-count
                project_chat(chat, count)
            })
            .collect();
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-for-each-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-return-chat-details
        Ok(details)
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-return-chat-details
    }

    /// Projects one existing chat with its message count.
    async fn project_one(
        &self,
        scope: &AccessScope,
        chat: Chat,
    ) -> Result<ChatDetail, DomainError> {
        let mut details = self.project(scope, vec![chat]).await?;
        details
            .pop()
            .ok_or_else(|| DomainError::Internal("chat projection lost its chat".to_owned()))
    }

    /// `POST /v1/chats`.
    ///
    /// # Errors
    ///
    /// `InvalidTitle`, `InvalidModel`, `AuthzDenied`, `PdpUnavailable`,
    /// `PolicyResolution`, `Database`.
    // @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-create-chat:p1
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-title-present
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-validate-create-title
        let title = title.map(validate_title).transpose()?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-validate-create-title
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-title-present

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-authorize-create
        let scope = self.authz.chat_scope(ctx, ChatAction::Create, None).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-authorize-create

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-resolve-model
        let model = self.resolve_model(ctx, model).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-resolve-model

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-insert-chat
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-insert-out-of-scope
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-insert-denied
        let now = now_micros();
        let chat = Chat {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model,
            title,
            is_temporary: false,
            created_at: now,
            updated_at: now,
        };
        // The repository maps a scope refusal to `AuthzDenied` (403).
        self.chats.insert(&scope, &chat).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-insert-denied
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-insert-out-of-scope
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-insert-chat

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-project-created
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-if-new-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-new-chat-count
        Ok(project_chat(chat, 0))
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-new-chat-count
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-if-new-chat
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-project-created
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }`, `AuthzDenied`, `PdpUnavailable`, `Database`.
    pub async fn get_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-authorize-read
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Read, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-authorize-read

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-load-chat-read
        let chat = self.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-load-chat-read

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-project-read
        self.project_one(&scope, chat).await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-project-read
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    ///
    /// `InvalidQuery`, `AuthzDenied`, `PdpUnavailable`, `Database`.
    // @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-read-list-rename:p1
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-authorize-list-chats
        let scope = self.authz.chat_scope(ctx, ChatAction::List, None).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-authorize-list-chats

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-page-chats
        let page = self.chats.page(&scope, query).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-page-chats

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-project-chat-page
        let items = self.project(&scope, page.items).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-project-chat-page

        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PATCH /v1/chats/{id}`.
    ///
    /// # Errors
    ///
    /// `InvalidTitle`, `NotFound { Chat }`, `AuthzDenied`, `PdpUnavailable`,
    /// `Database`.
    pub async fn rename_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-validate-rename-title
        let title = validate_title(title)?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-validate-rename-title

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-authorize-update
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Update, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-authorize-update

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-load-chat-update
        self.chats.load(&scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-load-chat-update

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-update-title
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-rename-active
        let affected = self
            .chats
            .rename(&scope, chat_id, &title, now_micros())
            .await?;
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-rename-active
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-update-title

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-if-update-lost
        if affected == 0 {
            // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-return-update-404
            return Err(DomainError::NotFound {
                resource: ResourceKind::Chat,
            });
            // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-return-update-404
        }
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-if-update-lost

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-project-updated
        let chat = self.chats.load(&scope, chat_id).await?;
        self.project_one(&scope, chat).await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-project-updated
    }

    /// `DELETE /v1/chats/{id}`: soft delete and chat-cleanup enqueue in one
    /// transaction.
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }`, `CleanupPayloadTooLarge`, `AuthzDenied`,
    /// `PdpUnavailable`, `Database`.
    // @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-delete-chat:p1
    pub async fn delete_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-authorize-delete
        let scope = self
            .authz
            .chat_scope(ctx, ChatAction::Delete, Some(chat_id))
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-authorize-delete

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-soft-delete
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-if-delete-error
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-return-delete-error
        // The error leaves as-is; `api::rest::error_mapping` builds the
        // `Problem` (404 chat, 400 cleanup payload too large, 500 internal).
        self.chats
            .soft_delete_with_cleanup(&scope, chat_id)
            .await
            .inspect_err(|error| {
                if matches!(error, DomainError::Internal(_) | DomainError::Database(_)) {
                    tracing::error!(%chat_id, %error, "chat delete failed");
                }
            })
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-return-delete-error
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-if-delete-error
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-soft-delete
    }
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
