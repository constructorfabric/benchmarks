//! Scoped repositories over the Secure ORM entities.
//!
//! A chat is loaded only through a query constrained by the `AccessScope` (tenant and
//! owner) and `deleted_at IS NULL`. Child rows (`messages`, `chat_turns`, `attachments`,
//! `message_attachments`, `thread_summaries`, `chat_vector_stores`) are reachable only
//! through a [`ChatRecord`], which exists only after such a query, and always with
//! `tenant_id` in the filter. `chat_vector_stores` has no lookup by primary key or by
//! `vector_store_id`.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Set};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::chats::ChatDetail;
use crate::domain::error::{DomainError, ResourceKind};
use crate::infra::storage::chat_page::{self, TimeStorage};
use crate::infra::storage::entity::{attachment, chat, chat_vector_store, message};

/// A chat row resolved through an owner-scoped query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl ChatRecord {
    /// Project the row and its `message_count` into the `ChatDetail` read model: `is_temporary`
    /// is `false`, no owner, tenant or deletion data.
    #[must_use]
    pub fn into_detail(self, message_count: i64) -> ChatDetail {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-required
        // the title stays `None` for an untitled chat; the wire layer omits it
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-no-internal
        // `user_id`, `tenant_id` and `deleted_at` of the row are not part of the read model
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-no-messages
        // no message is embedded: the history is loaded through the messages endpoint
        let detail = ChatDetail {
            id: self.id,
            model: self.model,
            title: self.title,
            is_temporary: false,
            message_count,
            created_at: self.created_at,
            updated_at: self.updated_at,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-no-messages
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-no-internal
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-required
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-return
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-return
        // the unchanged selected model is part of every detail
        detail
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-return
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-return
    }
}

/// Input of a chat insert. `tenant_id` and `user_id` come from the security context.
#[derive(Debug, Clone)]
pub struct NewChat {
    pub id: Uuid,
    pub model: String,
    pub title: Option<String>,
}

/// Input of a message insert. `tenant_id` and `chat_id` come from the resolved chat.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: String,
    pub content: String,
}

/// A message row.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: String,
    pub content: String,
    pub features_used: Value,
    pub created_at: OffsetDateTime,
}

/// A chat vector store row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub vector_store_id: Option<String>,
    pub provider: String,
}

/// Chat repository: owner-scoped chats and the chat-scoped child access.
///
/// Every method takes the caller's `runner: &impl DBRunner`, never a connection of its own, so
/// one body serves both a pooled `DbConn` and a `DBProvider::transaction` handle. A domain write
/// can therefore join the transaction of its outbox enqueue and commit or roll back with it.
pub struct ChatRepo;

/// The not-found result of a chat query: the same for a missing, soft-deleted, foreign-tenant
/// or foreign-owner chat.
fn chat_not_found(chat_id: Option<Uuid>) -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Chat,
        id: chat_id.map(|id| id.to_string()),
    }
}

fn chat_record(row: chat::Model) -> ChatRecord {
    ChatRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        user_id: row.user_id,
        model: row.model,
        title: row.title,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn message_record(row: message::Model) -> MessageRecord {
    MessageRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        request_id: row.request_id,
        role: row.role,
        content: row.content,
        features_used: row.features_used,
        created_at: row.created_at,
    }
}

/// The current time truncated to microseconds, the precision every supported database keeps, so
/// a value returned by a write equals the value stored.
fn now_micros() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}

/// The live (not soft-deleted) chat with `chat_id`.
fn live_chat(chat_id: Uuid) -> Condition {
    // @cpt-begin:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-deleted-terminal
    // a Deleted chat is terminal: no operation finds it, so it answers 404 and nothing changes
    // (no hard purge exists in P1)
    Condition::all()
        .add(chat::Column::Id.eq(chat_id))
        .add(chat::Column::DeletedAt.is_null())
    // @cpt-end:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-deleted-terminal
}

/// The scope of the child rows of a resolved chat: its tenant. The owner check already
/// happened when the chat was resolved through the owner-scoped query.
pub(crate) fn child_scope(chat: &ChatRecord) -> AccessScope {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-children
    // a child row is reachable only through a chat resolved by an owner-scoped live-chat query,
    // so the sub-resources of a soft-deleted chat are unreachable without any child row being
    // modified
    AccessScope::for_tenant(chat.tenant_id)
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-children
}

impl ChatRepo {
    /// Insert a chat; `tenant_id` and `user_id` come from `ctx`, never from request input,
    /// and the insert is validated against `scope`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the scope does not cover the row or the database fails.
    pub async fn create_chat(
        runner: &impl DBRunner,
        scope: &AccessScope,
        ctx: &SecurityContext,
        new: NewChat,
    ) -> Result<ChatRecord, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-create
        let now = now_micros();
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-create
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-writers
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-create-owner
        // `tenant_id` and `user_id` come from the security context, never from the request body
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-single-write
        // `model` is written here and in no other statement of the feature
        let row = chat::ActiveModel {
            id: Set(new.id),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(new.model),
            title: Set(new.title),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-single-write
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-create-owner
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-writers
        let inserted = secure_insert::<chat::Entity>(row, scope, runner).await?;
        Ok(chat_record(inserted))
    }

    /// Load a chat through a query constrained by `scope` and `deleted_at IS NULL`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` for a missing, soft-deleted, foreign-tenant or
    /// foreign-owner chat (all indistinguishable).
    pub async fn load_chat(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatRecord, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-load-chat
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-chat-scoped
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-load-chat
        let found = chat::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(live_chat(chat_id))
            .one(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-load-chat
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-chat-scoped
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-load-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-no-row
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-return-not-found
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-chat-missing
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-404
        let row = found.ok_or_else(|| chat_not_found(Some(chat_id)))?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-chat-missing
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-return-not-found
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-no-row
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-return
        Ok(chat_record(row))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-return
    }

    /// List the caller's live chats, most recently updated first.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn list_chats(
        runner: &impl DBRunner,
        scope: &AccessScope,
    ) -> Result<Vec<ChatRecord>, DomainError> {
        let rows = chat::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()))
            .order_by(chat::Column::UpdatedAt, Order::Desc)
            .order_by(chat::Column::Id, Order::Asc)
            .all(runner)
            .await?;
        Ok(rows.into_iter().map(chat_record).collect())
    }

    /// Set a new title on a chat the scope covers.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    pub async fn rename_chat(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<(), DomainError> {
        let affected = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.to_owned()))
            .col_expr(
                chat::Column::UpdatedAt,
                Expr::value(OffsetDateTime::now_utc()),
            )
            .filter(live_chat(chat_id))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?
            .rows_affected;
        if affected == 0 {
            return Err(chat_not_found(Some(chat_id)));
        }
        Ok(())
    }

    /// Soft-delete a chat the scope covers.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    pub async fn soft_delete_chat(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let now = OffsetDateTime::now_utc();
        let affected = chat::Entity::update_many()
            .col_expr(chat::Column::DeletedAt, Expr::value(now))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live_chat(chat_id))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?
            .rows_affected;
        if affected == 0 {
            return Err(chat_not_found(Some(chat_id)));
        }
        Ok(())
    }

    /// `message_count` of a resolved chat: the non-deleted `messages` rows of the chat, all
    /// roles included, filtered by `chat_id` and `tenant_id`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn count_messages(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<i64, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-query
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-scoped
        // `messages` is reached only through the id of a chat resolved by an owner-scoped query,
        // and the scope of the child rows always carries the tenant
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-count
        // the count of the non-deleted messages: a completed turn wrote two (user and
        // assistant), a failed turn or a cancelled turn without text one, a cancelled turn with
        // a partial message two, a rejected or rolled-back send and a replay none
        let count = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat.id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .count(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-count
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-scoped
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-query
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-stable
        // the count is derived at read time from the messages, never stored: a rename does not
        // touch `messages`, so it cannot change the count
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-stable
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-return
        i64::try_from(count)
            .map_err(|_| DomainError::internal("the message count does not fit in an i64"))
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-message-count:p1:inst-count-return
    }

    /// One page of the caller's live chats for a validated `OData` query: constrained by `scope`
    /// and `deleted_at IS NULL`, default order `updated_at desc`, every effective order made
    /// total by an `id` tiebreaker in the primary direction, page size 20 by default and at
    /// most 100. `storage` is how the engine stores the timestamps (see [`TimeStorage`]).
    ///
    /// # Errors
    ///
    /// [`DomainError::ODataQuery`] for a filter on an unknown field or with an operator
    /// outside the field's set, an order outside the six allowed values, or a cursor that does
    /// not match the query; another [`DomainError`] when the database fails.
    pub async fn list_chat_page(
        runner: &impl DBRunner,
        scope: &AccessScope,
        query: &ODataQuery,
        storage: TimeStorage,
    ) -> Result<Page<ChatRecord>, DomainError> {
        // `deleted_at IS NULL` and the scope are applied before any filter of the query
        let select = chat::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()));
        let page = chat_page::paginate(runner, select, query, storage).await?;
        Ok(page.map_items(chat_record))
    }

    /// Write the trimmed `title` and `updated_at = now` of a live chat the scope covers, in one
    /// UPDATE that writes no other column, and return the row as written.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    pub async fn update_title(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatRecord, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-write
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-rename
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-patch
        // @cpt-begin:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-rename
        // one UPDATE writes `title` and `updated_at` and no other column (`model` and
        // `is_temporary` included), constrained by the scope, the chat id and `deleted_at IS NULL`
        let affected = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.to_owned()))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now_micros()))
            .filter(live_chat(chat_id))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?
            .rows_affected;
        // @cpt-end:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-rename
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-patch
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-rename
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-write
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-no-row
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-404
        // the chat was soft-deleted (or never covered by the scope) between the load and the write
        if affected == 0 {
            return Err(chat_not_found(Some(chat_id)));
        }
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-no-row
        Self::load_chat(runner, scope, chat_id).await
    }

    /// Move the `updated_at` of a live chat the scope covers forward to now: the write of the
    /// message-activity contract (a sent message, a retry and an edit call it in the same
    /// transaction as the message write). Returns the new `updated_at`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat.
    pub async fn touch_chat(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<OffsetDateTime, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-message-contract
        // @cpt-begin:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-activity
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-downgrade
        // the caller runs this inside the transaction of its message write, never separately;
        // the write sets `updated_at` only: no quota-driven downgrade ever writes `model`
        let now = now_micros();
        let affected = chat::Entity::update_many()
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live_chat(chat_id))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?
            .rows_affected;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-downgrade
        // @cpt-end:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-activity
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-message-contract
        if affected == 0 {
            return Err(chat_not_found(Some(chat_id)));
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-return
        Ok(now)
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-return
    }

    /// Soft-delete a live chat the scope covers: `deleted_at` and `updated_at` are set to the
    /// same current time, which is returned (the `chat_deleted_at` of the cleanup payload).
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Chat }` when the scope covers no such live chat (another delete
    /// committed first).
    pub async fn soft_delete_chat_at(
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<OffsetDateTime, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-soft-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-delete
        // `deleted_at` and `updated_at` get the same time in one UPDATE, guarded by the scope,
        // the chat id and `deleted_at IS NULL`, so exactly one delete wins
        let now = now_micros();
        let affected = chat::Entity::update_many()
            .col_expr(chat::Column::DeletedAt, Expr::value(now))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live_chat(chat_id))
            .secure()
            .scope_with(scope)
            .exec(runner)
            .await?
            .rows_affected;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-delete
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-soft-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-already-deleted
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-already-deleted
        // the caller rolls back, which the flow maps to 404
        if affected == 0 {
            return Err(chat_not_found(Some(chat_id)));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-already-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-already-deleted
        Ok(now)
    }

    /// Mark the attachments of a resolved chat for provider cleanup: `cleanup_status` becomes
    /// `pending` for the rows of this chat and tenant that have no cleanup status yet (rows
    /// already `pending`, `done` or `failed` keep their value); nothing else is written.
    /// Returns the number of rows marked.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn mark_attachments_cleanup_pending(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<u64, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-mark-attachments
        // documents and images alike; the only allowed transitions of the cleanup state machine
        // start at `pending`, so a row that already has a status keeps it
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat.id))
                    .add(attachment::Column::CleanupStatus.is_null()),
            )
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?
            .rows_affected;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-mark-attachments
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-children-untouched
        // `messages`, `chat_turns`, `message_attachments`, `message_reactions` and the
        // `deleted_at` of every attachment are not written: they become unreachable because
        // every read goes through the chat
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-children-untouched
        Ok(affected)
    }

    /// Insert a message into a resolved chat (`tenant_id` and `chat_id` from the chat).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn add_message(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        new: NewMessage,
    ) -> Result<MessageRecord, DomainError> {
        // `tenant_id` and `chat_id` come from the resolved chat, never from request input
        let row = message::ActiveModel {
            id: Set(new.id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(new.request_id),
            role: Set(new.role),
            content: Set(new.content),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(None),
            request_kind: Set("chat".to_owned()),
            features_used: Set(Value::Array(Vec::new())),
            input_tokens: Set(0),
            output_tokens: Set(0),
            cache_read_input_tokens: Set(0),
            cache_write_input_tokens: Set(0),
            reasoning_tokens: Set(0),
            model: Set(None),
            is_compressed: Set(false),
            created_at: Set(OffsetDateTime::now_utc()),
            deleted_at: Set(None),
        };
        let inserted = secure_insert::<message::Entity>(row, &child_scope(chat), runner).await?;
        Ok(message_record(inserted))
    }

    /// The live messages of a resolved chat in `(created_at, id)` order, filtered by
    /// `chat_id` and `tenant_id`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn list_messages(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<Vec<MessageRecord>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-children
        // child rows are reached only through the `chat_id` of a resolved chat, and the scope
        // always carries the tenant
        let rows = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat.id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .order_by(message::Column::CreatedAt, Order::Asc)
            .order_by(message::Column::Id, Order::Asc)
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-children
        Ok(rows.into_iter().map(message_record).collect())
    }

    /// The vector store row of a resolved chat, filtered by `(tenant_id, chat_id)`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn vector_store(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<Option<VectorStoreRecord>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-vector-store-rule
        // there is no lookup by primary key or by `vector_store_id`: the row is reached through
        // the resolved chat and always filtered by `tenant_id`
        let row = chat_vector_store::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat.id)))
            .one(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-persistence:p1:inst-scope-vector-store-rule
        Ok(row.map(|row| VectorStoreRecord {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            vector_store_id: row.vector_store_id,
            provider: row.provider,
        }))
    }
}

#[cfg(test)]
#[path = "repo_tests.rs"]
mod repo_tests;

#[cfg(test)]
#[path = "chat_repo_tests.rs"]
mod chat_repo_tests;
