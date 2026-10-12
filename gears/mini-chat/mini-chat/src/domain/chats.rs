//! Chats service (DESIGN 3.3 "Create / List / Get / Update / Delete Chat").
//!
//! Every operation asks the PDP first (`create`, `list`, `read`, `update`,
//! `delete` on the chat resource) and works with the compiled scope narrowed
//! to the subject, so another user's chat is indistinguishable from an
//! unknown one (404). Deleting a chat is one transaction: soft delete, attachment
//! cleanup marking and the chat-cleanup outbox enqueue commit together.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::TxConfig;
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{actions, chat_scope};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{ChatCleanupPort, ChatRepository, ModelPolicyPort};

/// Maximum chat title length in characters (after trimming).
pub const TITLE_MAX_CHARS: usize = 255;

/// Default page size of the list endpoints.
pub const PAGE_DEFAULT_LIMIT: u64 = 20;

/// Largest page size of the list endpoints; a larger request is clamped.
pub const PAGE_MAX_LIMIT: u64 = 100;

/// A live chat as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRow {
    /// Chat id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning user.
    pub user_id: Uuid,
    /// Selected model, immutable for the chat lifetime.
    pub model: String,
    /// Title, absent for an untitled chat.
    pub title: Option<String>,
    /// Temporary-chat flag (always `false` in P1).
    pub is_temporary: bool,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub updated_at: OffsetDateTime,
}

/// A chat to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChat {
    /// Chat id.
    pub id: Uuid,
    /// Owning tenant (from the security context).
    pub tenant_id: Uuid,
    /// Owning user (from the security context).
    pub user_id: Uuid,
    /// Resolved model.
    pub model: String,
    /// Trimmed title.
    pub title: Option<String>,
    /// Creation (and first activity) time.
    pub now: OffsetDateTime,
}

/// A chat with its message count (the `ChatDetail` of the API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    /// Chat id.
    pub id: Uuid,
    /// Selected model.
    pub model: String,
    /// Title, absent for an untitled chat.
    pub title: Option<String>,
    /// Temporary-chat flag.
    pub is_temporary: bool,
    /// Number of non-soft-deleted messages.
    pub message_count: i64,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub updated_at: OffsetDateTime,
}

impl ChatView {
    fn new(row: ChatRow, message_count: i64) -> Self {
        Self {
            id: row.id,
            model: row.model,
            title: row.title,
            is_temporary: row.is_temporary,
            message_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// Payload of the chat-cleanup message of a soft-deleted chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCleanupEvent {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The soft-deleted chat.
    pub chat_id: Uuid,
    /// The stored `deleted_at` of the chat.
    pub chat_deleted_at: OffsetDateTime,
}

/// Fields of the chat list that `$filter` and `$orderby` accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatFilterField {
    /// `id` (UUID).
    Id,
    /// `title` (string).
    Title,
    /// `updated_at` (instant).
    UpdatedAt,
}

impl FilterField for ChatFilterField {
    const FIELDS: &'static [Self] = &[Self::Id, Self::Title, Self::UpdatedAt];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
            Self::UpdatedAt => "updated_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
            Self::UpdatedAt => FieldKind::DateTimeUtc,
        }
    }
}

/// Validates and trims a chat title (1..=255 characters after trimming).
///
/// # Errors
/// `InvalidTitle` when the trimmed title is empty or too long.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    let chars = trimmed.chars().count();
    if chars == 0 {
        return Err(DomainError::InvalidTitle {
            detail: "title must not be empty".to_owned(),
        });
    }
    if chars > TITLE_MAX_CHARS {
        return Err(DomainError::InvalidTitle {
            detail: format!("title must be at most {TITLE_MAX_CHARS} characters"),
        });
    }
    Ok(trimmed.to_owned())
}

/// Picks the model of a new chat from the catalog: the requested one (it must
/// be enabled) or the first enabled default, else the first enabled entry.
fn resolve_model(
    catalog: &[ModelCatalogEntry],
    requested: Option<&str>,
) -> Result<String, DomainError> {
    let mut enabled = catalog.iter().filter(|entry| entry.enabled);
    let chosen = if let Some(id) = requested {
        enabled.find(|entry| entry.id == id)
    } else {
        let preferred = catalog.iter().find(|entry| {
            entry.enabled
                && entry
                    .preference
                    .as_ref()
                    .is_some_and(|preference| preference.is_default)
        });
        preferred.or_else(|| enabled.next())
    };
    chosen
        .map(|entry| entry.id.clone())
        .ok_or(DomainError::InvalidModel)
}

/// Chat CRUD, chat list and chat deletion.
pub struct ChatsService {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyPort>,
    cleanup: Arc<dyn ChatCleanupPort>,
    enforcer: PolicyEnforcer,
}

impl ChatsService {
    /// Creates the service.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static, C: ChatCleanupPort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        cleanup: Arc<C>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            db,
            policy,
            cleanup,
            enforcer,
        }
    }

    /// Creates a chat for the caller.
    ///
    /// Order: title validation, PDP `create`, model resolution against the
    /// current catalog, insert.
    ///
    /// # Errors
    /// `InvalidTitle`, `AccessDenied` / `ServiceUnavailable` from the PDP,
    /// `InvalidModel` for an unknown or disabled model, `Internal`.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<ChatView, DomainError> {
        let title = title.map(validate_title).transpose()?;
        let scope = chat_scope(&self.enforcer, ctx, actions::CREATE, None).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = resolve_model(&snapshot.model_catalog, model)?;

        let conn = self.db.conn()?;
        let row = conn
            .insert_chat(
                &scope,
                NewChat {
                    id: Uuid::new_v4(),
                    tenant_id: ctx.subject_tenant_id(),
                    user_id: ctx.subject_id(),
                    model,
                    title,
                    now: now_utc(),
                },
            )
            .await?;
        Ok(ChatView::new(row, 0))
    }

    /// One chat of the caller.
    ///
    /// # Errors
    /// `NotFound` for an unknown, foreign or deleted chat, PDP errors, `Internal`.
    pub async fn get_chat(&self, ctx: &SecurityContext, id: Uuid) -> Result<ChatView, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::READ, Some(id)).await?;
        let conn = self.db.conn()?;
        let row = conn
            .find_live_chat(&scope, id)
            .await?
            .ok_or_else(|| chat_not_found(id))?;
        let counts = conn
            .count_live_messages(&scope.tenant_only(), &[id])
            .await?;
        let count = counts.get(&id).copied().unwrap_or(0);
        Ok(ChatView::new(row, count))
    }

    /// A page of the caller's chats; the default order is `updated_at desc`
    /// with `id` as the tiebreaker.
    ///
    /// # Errors
    /// `OData` for a bad `$filter` / `$orderby` / cursor / page size, PDP errors,
    /// `Internal`.
    pub async fn list_chats(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ChatView>, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::LIST, None).await?;
        let query = with_default_order(query, ChatFilterField::UpdatedAt.name(), SortDir::Desc);
        let conn = self.db.conn()?;
        let page = conn.list_live_chats(&scope, &query).await?;
        let ids: Vec<Uuid> = page.items.iter().map(|row| row.id).collect();
        let counts = conn.count_live_messages(&scope.tenant_only(), &ids).await?;
        let items = page
            .items
            .into_iter()
            .map(|row| {
                let count = counts.get(&row.id).copied().unwrap_or(0);
                ChatView::new(row, count)
            })
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// Renames a chat; only `title` and `updated_at` change.
    ///
    /// # Errors
    /// `InvalidTitle`, `NotFound`, PDP errors, `Internal`.
    pub async fn update_title(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        title: &str,
    ) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = chat_scope(&self.enforcer, ctx, actions::UPDATE, Some(id)).await?;
        let conn = self.db.conn()?;
        let row = conn
            .update_chat_title(&scope, id, &title, now_utc())
            .await?
            .ok_or_else(|| chat_not_found(id))?;
        let counts = conn
            .count_live_messages(&scope.tenant_only(), &[id])
            .await?;
        let count = counts.get(&id).copied().unwrap_or(0);
        Ok(ChatView::new(row, count))
    }

    /// Soft-deletes a chat and enqueues its provider cleanup in one
    /// transaction. Messages, turns, attachments and reactions keep their rows.
    ///
    /// # Errors
    /// `NotFound`, `OutboxPayloadTooLarge`, PDP errors, `Internal` (the whole
    /// transaction rolls back on any failure).
    pub async fn delete_chat(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::DELETE, Some(id)).await?;
        let children = scope.tenant_only();
        let now = now_utc();

        // The first statement of the transaction is the write: a SQLite
        // transaction that starts with a read and then upgrades to a write
        // fails with SQLITE_BUSY_SNAPSHOT when another writer (the outbox
        // sequencer) committed in between. Retryable contention errors rerun
        // the whole transaction.
        let wake = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let scope = scope.clone();
                let children = children.clone();
                let cleanup = Arc::clone(&self.cleanup);
                Box::pin(async move {
                    let row = tx
                        .soft_delete_chat(&scope, id, now)
                        .await?
                        .ok_or_else(|| chat_not_found(id))?;
                    tx.mark_attachments_for_cleanup(&children, id).await?;
                    // Last step: the wake only exists once the message is enqueued.
                    cleanup
                        .enqueue_chat_cleanup(
                            tx,
                            &ChatCleanupEvent {
                                tenant_id: row.tenant_id,
                                chat_id: id,
                                chat_deleted_at: now,
                            },
                        )
                        .await
                })
            })
            .await?;
        // Wake the outbox sequencers now that the rows are committed.
        wake.fire();
        Ok(())
    }
}

/// Not-found error of a chat (unknown, foreign or soft-deleted).
pub(crate) fn chat_not_found(id: Uuid) -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Chat,
        id: id.to_string(),
    }
}

/// The query with `field dir` as its order when neither `$orderby` nor a
/// cursor (which carries its own order) fixes one.
pub(crate) fn with_default_order(query: &ODataQuery, field: &str, dir: SortDir) -> ODataQuery {
    let mut query = query.clone();
    if query.order.0.is_empty() && query.cursor.is_none() {
        query.order = ODataOrderBy(vec![OrderKey {
            field: field.to_owned(),
            dir,
        }]);
    }
    query
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
