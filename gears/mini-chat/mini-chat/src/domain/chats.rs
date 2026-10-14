//! Chat management (feature `cpt-cf-mini-chat-feature-chat-management`): the pure rules
//! (title validation, model resolution), the `ChatDetail` read model, the persistence and
//! chat-cleanup ports and the service behind the five chat endpoints.
//!
//! The persistence of the service is the [`ChatStore`] port, implemented by the infrastructure
//! layer over the database provider; the chat-cleanup outbox queue is the [`ChatCleanupQueue`]
//! port. The domain layer imports nothing from the infrastructure or API layers.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::PolicySnapshot;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{Authorization, Operation, PepService};
use crate::domain::catalog_rules::{default_model, validate_selected_model};
use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;

/// The longest title in characters, counted after trimming.
pub const TITLE_MAX_CHARS: usize = 255;

/// The `reason` of the chat-cleanup outbox payload written by the chat delete transaction.
pub const CHAT_CLEANUP_REASON: &str = "chat_soft_delete";

/// The chat as every create, get, update and list item exposes it (`ChatDetail`): metadata and
/// the derived `message_count`; no owner, tenant or deletion data.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDetail {
    pub id: Uuid,
    pub model: String,
    /// `None` for an untitled chat.
    pub title: Option<String>,
    /// Always `false` in P1.
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The input of `POST /v1/chats` after JSON extraction; both fields are optional.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateChat {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// The validated, resolved values of a new chat; the owner and the tenant come from the
/// security context at the insert, never from here.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDraft {
    pub id: Uuid,
    pub model: String,
    /// The trimmed title, `None` for an untitled chat.
    pub title: Option<String>,
}

/// What the chat delete transaction hands to the chat-cleanup queue: the chat and the
/// `deleted_at` its soft-delete wrote. The queue adds the stable `system_request_id`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCleanupMessage {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub chat_deleted_at: OffsetDateTime,
}

/// Port of the chat-cleanup outbox queue, implemented by the transactional enqueuer of the
/// infrastructure layer.
#[async_trait]
pub trait ChatCleanupQueue: Send + Sync {
    /// Enqueue exactly one chat-cleanup message inside the caller's transaction `tx`; the
    /// returned [`Wake`] is fired after the commit.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge { site: ChatDelete }` for an oversized payload; `Internal`
    /// when the outbox is not running or the database fails.
    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        message: &ChatCleanupMessage,
    ) -> Result<Wake, DomainError>;
}

/// Port of the chat persistence, implemented by the infrastructure layer over the database
/// provider. Every method takes the owner-scoped [`AccessScope`] of the caller and reaches the
/// rows only through it; the not-found result is the same for a missing, soft-deleted,
/// foreign-owner and foreign-tenant chat.
#[async_trait]
pub trait ChatStore: Send + Sync {
    /// Insert a chat (`tenant_id` and `user_id` from `ctx`) and project it with
    /// `message_count` equal to `0`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the scope does not cover the row or the database fails.
    async fn insert_chat(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        draft: ChatDraft,
    ) -> Result<ChatDetail, DomainError>;

    /// Load a live chat the scope covers and project it with its `message_count`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    async fn load_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError>;

    /// One page of the caller's live chats for the `OData` query, each projected with its
    /// `message_count`.
    ///
    /// # Errors
    ///
    /// [`DomainError::ODataQuery`] for a filter, order or cursor the list query rejects.
    async fn list_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError>;

    /// Write the validated `title` and `updated_at = now` of a live chat the scope covers and
    /// project the row as written.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    async fn rename_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError>;

    /// The delete transaction: soft-delete the chat, mark its attachments for cleanup and
    /// enqueue the chat-cleanup message through `cleanup`, all committed or rolled back
    /// together.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` for a chat that is not live (nothing is enqueued),
    /// `OutboxPayloadTooLarge { site: ChatDelete }` for an oversized payload, `Internal` for
    /// any other failure; the transaction is rolled back in every case.
    async fn delete_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        cleanup: Arc<dyn ChatCleanupQueue>,
    ) -> Result<(), DomainError>;
}

/// Title validation: `None` (absent or `null`) is an untitled chat; a present value is trimmed
/// and must have 1 to 255 characters.
///
/// # Errors
///
/// [`DomainError::InvalidTitle`] for an empty, whitespace-only or too long title.
pub fn validate_title(raw: Option<&str>) -> Result<Option<String>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-absent
    let Some(raw) = raw else {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return-none
        return Ok(None);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return-none
    };
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-absent
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-trim
    let trimmed = raw.trim();
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-trim
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-count
    // characters, not bytes
    let length = trimmed.chars().count();
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-count
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-out-of-range
    if length == 0 || length > TITLE_MAX_CHARS {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return-error
        return Err(DomainError::InvalidTitle);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return-error
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-out-of-range
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return
    Ok(Some(trimmed.to_owned()))
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-title-validation:p1:inst-title-return
}

/// Chat model resolution: a supplied model must be an enabled catalog entry; an absent one
/// resolves to the first enabled `is_default` entry, else the first enabled entry (the tier is
/// not considered).
///
/// # Errors
///
/// [`DomainError::InvalidModel`] for an unknown, disabled or empty model, or when no enabled
/// entry exists and none was supplied.
pub fn resolve_chat_model(
    snapshot: &PolicySnapshot,
    requested: Option<&str>,
) -> Result<String, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-supplied
    if let Some(requested) = requested {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-lookup
        // an entry that omits `enabled` counts as disabled, so it is not found here
        let found = validate_selected_model(snapshot, requested);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-lookup
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-not-selectable
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-invalid-supplied
        let entry = found?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-invalid-supplied
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-not-selectable
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-supplied
        return Ok(entry.id.clone());
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-supplied
    }
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-supplied
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-default
    let default = default_model(snapshot);
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-default
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-none-enabled
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-invalid-default
    let entry = default.ok_or_else(|| DomainError::InvalidModel {
        model: String::new(),
    })?;
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-invalid-default
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-none-enabled
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-default
    Ok(entry.id.clone())
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-default
}

/// The scope of a chat authorization; a decision-only outcome cannot address a chat.
pub(crate) fn scope_of(authorization: Authorization) -> Result<AccessScope, DomainError> {
    match authorization {
        Authorization::Scoped(scope) => Ok(scope),
        Authorization::DecisionOnly => Err(DomainError::internal(
            "a chat operation was authorized without an access scope",
        )),
    }
}

/// The service behind the five chat endpoints.
#[domain_model]
pub struct ChatsService {
    pub pep: Arc<PepService>,
    pub gateway: Arc<dyn ModelPolicyGateway>,
    pub store: Arc<dyn ChatStore>,
    pub cleanup: Arc<dyn ChatCleanupQueue>,
}

impl ChatsService {
    /// Create the service over its collaborators; the database provider is the persistence
    /// port ([`ChatStore`]) through the implementation of the infrastructure layer.
    #[must_use]
    pub fn new(
        pep: Arc<PepService>,
        gateway: Arc<dyn ModelPolicyGateway>,
        db: Arc<DBProvider<DomainError>>,
        cleanup: Arc<dyn ChatCleanupQueue>,
    ) -> Self {
        Self {
            pep,
            gateway,
            store: db,
            cleanup,
        }
    }

    /// Authorize a chat operation through the PEP and return the owner-scoped
    /// [`AccessScope`] of the caller.
    async fn admit(
        &self,
        ctx: &SecurityContext,
        operation: Operation,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-matrix
        // create, list, read, update and delete rows of the Chat type, constraints required
        let admission = self.pep.authorize(ctx, operation, chat_id).await;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-matrix
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-scope
        // the PEP adds the owner predicate to the tenant predicate of the PDP; every database
        // access of the feature runs through this scope
        let scope = scope_of(admission?);
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-scope
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-return
        scope
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-return
    }

    /// `POST /v1/chats`: validate the title, authorize, resolve the model, insert.
    ///
    /// # Errors
    ///
    /// `InvalidTitle` (before any authorization or model lookup), an admission refusal,
    /// `PolicyUnavailable` when the policy plugin fails, `InvalidModel`, or a database failure.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        input: CreateChat,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-validate-title
        let title = validate_title(input.title.as_deref());
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-validate-title
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-title-invalid
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-400-title
        let title = title?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-400-title
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-title-invalid

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authorize
        let scope = self.admit(ctx, Operation::CreateChat, None).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authorize
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authz-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-refusal
        let scope = scope?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authz-refused

        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-read-snapshot
        // no local cache: every create asks the plugin
        let snapshot = self.gateway.current_snapshot(ctx.subject_id()).await;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-read-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-plugin-error
        // the error mapping turns a plugin failure into a generic 500
        let snapshot = snapshot?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-return-plugin-error
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-resolution:p1:inst-model-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-resolve-model
        let model = resolve_chat_model(&snapshot, input.model.as_deref());
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-resolve-model
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-model-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-model-error
        let model = model?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-model-error
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-model-failed

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-insert
        let draft = ChatDraft {
            id: Uuid::new_v4(),
            model,
            title,
        };
        self.store.insert_chat(&scope, ctx, draft).await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-insert
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { resource: Chat }`, or a database failure.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-admission
        let scope = self.admit(ctx, Operation::GetChat, Some(chat_id)).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return-refusal
        // a refusal, or the not-found of the owner-scoped load, is returned as it is
        let scope = scope?;
        self.store.load_chat(&scope, chat_id).await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-refused
    }

    /// `GET /v1/chats`: one page of the caller's live chats.
    ///
    /// # Errors
    ///
    /// An admission refusal, [`DomainError::ODataQuery`] for a rejected filter, order or
    /// cursor, or a database failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-admission
        let scope = self.admit(ctx, Operation::ListChats, None).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-refusal
        let scope = scope?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-refused
        self.store.list_chats(&scope, query).await
    }

    /// `PATCH /v1/chats/{id}`: validate the title, authorize, write `title` and `updated_at`.
    ///
    /// # Errors
    ///
    /// `InvalidTitle` (before the chat is loaded), an admission refusal, `NotFound`
    /// (including a chat soft-deleted between the load and the write), or a database failure.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-validate-title
        let validated = validate_title(Some(title));
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-validate-title
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-title-invalid
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-400-title
        let validated = validated?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-400-title
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-title-invalid
        let Some(title) = validated else {
            return Err(DomainError::internal(
                "a present title validated to an untitled chat",
            ));
        };

        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-admission
        let scope = self.admit(ctx, Operation::UpdateChat, Some(chat_id)).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-refusal
        let scope = scope?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-refused
        self.store.rename_chat(&scope, chat_id, &title).await
    }

    /// `DELETE /v1/chats/{id}`: the delete transaction.
    ///
    /// # Errors
    ///
    /// An admission refusal, `NotFound { resource: Chat }` (also when another delete committed
    /// first), `OutboxPayloadTooLarge { site: ChatDelete }`, or an internal failure; the
    /// transaction is rolled back in every failing case.
    pub async fn delete(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-admission
        let scope = self.admit(ctx, Operation::DeleteChat, Some(chat_id)).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-refusal
        let scope = scope?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-transaction
        let outcome = self
            .store
            .delete_chat(&scope, chat_id, Arc::clone(&self.cleanup))
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-transaction
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-lost-race
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-404
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-payload-too-large
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-400-payload
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-500
        // an already-deleted chat (404), an oversized payload (400) and any other failure (500,
        // the cause is logged by the error mapping and the detail stays generic) are returned
        // as the domain error of the rolled-back transaction
        outcome
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-500
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-failed
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-400-payload
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-payload-too-large
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-lost-race
    }
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod chats_tests;
