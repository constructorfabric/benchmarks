//! Chat CRUD service.

use std::sync::Arc;

use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::model::{ChatDetail, NewChat};
use super::repo::ChatRepository;
use super::title::normalize_title;
use crate::domain::authz::{Pep, actions};
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::models::resolve_chat_model;
use crate::domain::ports::ModelPolicyGateway;

/// Chat create / list / get / rename / delete.
///
/// Authorization comes first (except for title validation, which precedes it); the repository
/// then applies the compiled scope, so a chat that is missing, soft-deleted or owned by someone
/// else is indistinguishable: `ChatNotFound`.
#[domain_model]
pub struct ChatService {
    repo: Arc<dyn ChatRepository>,
    pep: Pep,
    policy: Arc<dyn ModelPolicyGateway>,
}

impl ChatService {
    /// Creates the service.
    #[must_use]
    pub fn new(
        repo: Arc<dyn ChatRepository>,
        pep: Pep,
        policy: Arc<dyn ModelPolicyGateway>,
    ) -> Self {
        Self { repo, pep, policy }
    }

    /// Creates a chat owned by the caller. The title is validated before the authorization check
    /// and the model lookup; the model defaults to the first enabled default entry of the catalog.
    ///
    /// # Errors
    /// `InvalidTitle`, `AuthzDenied`, `AuthzUnavailable`, `InvalidModel`, `PolicyUnavailable` or an
    /// internal error.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<ChatDetail, DomainError> {
        let title = title.map(normalize_title).transpose()?;
        let scope = self.pep.create_chat_scope(ctx).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = resolve_chat_model(&snapshot.model_catalog, model)?;
        let new_chat = NewChat {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            model,
            title,
            now: clock::now(),
        };
        self.repo.insert(&scope, new_chat).await
    }

    /// Lists the caller's chats.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, an `OData` query error or an internal error.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        let scope = self.pep.chat_scope(ctx, actions::LIST, None).await?;
        self.repo.list(&scope, query).await
    }

    /// Loads one of the caller's chats.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound` or an internal error.
    pub async fn get_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::READ, Some(chat_id))
            .await?;
        self.repo
            .find(&scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })
    }

    /// Renames one of the caller's chats; `chats.model` and every other field stay unchanged.
    ///
    /// # Errors
    /// `InvalidTitle`, `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound` or an internal error.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        let title = normalize_title(title)?;
        let scope = self
            .pep
            .chat_scope(ctx, actions::UPDATE, Some(chat_id))
            .await?;
        self.repo
            .rename(&scope, chat_id, &title, clock::now())
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })
    }

    /// Soft-deletes one of the caller's chats and schedules the provider cleanup.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `OutboxPayloadTooLarge` or an internal
    /// error.
    pub async fn delete_chat(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::DELETE, Some(chat_id))
            .await?;
        let deleted = self
            .repo
            .soft_delete(&scope, chat_id, Uuid::new_v4(), clock::now())
            .await?;
        if deleted {
            Ok(())
        } else {
            Err(DomainError::ChatNotFound { chat_id })
        }
    }
}
