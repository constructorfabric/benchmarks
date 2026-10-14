//! Message reads of the send pipeline and the messages list over the Secure ORM (slice
//! S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`): the message page, the
//! attachment and reaction batches of a page, the snapshot boundary, the recent messages of the
//! context and the thread summary.
//!
//! Every function takes the caller's `runner` and reaches its rows only through a [`ChatRecord`]
//! (a chat resolved by an owner-scoped query) and the tenant of that chat. On `SQLite`
//! timestamps are compared, ordered and paged only through the fixed-width sort key
//! ([`TimeStorage::Rfc3339Text`]), never as raw text; on `PostgreSQL` the native type is used;
//! `id` is always the tie-break.

use std::collections::BTreeMap;

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Set, Value};
use serde_json::Value as Json;
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::messages::{AttachmentRow, MessageRow, Reaction, ThumbnailRow};
use crate::domain::mutations::LinkedAttachment;
use crate::domain::streaming::{
    AttachmentFact, ContextMessage, ContextRole, SnapshotBoundary, ThreadSummaryRow,
};
use crate::infra::storage::entity::{
    attachment, message, message_attachment, message_reaction, thread_summary,
};
use crate::infra::storage::keyset_page::{time_bound, time_key};
use crate::infra::storage::message_page;
use crate::infra::storage::repo_impl::{ChatRecord, child_scope};

use crate::infra::storage::chat_page::TimeStorage;

/// The live messages of the chat: its `chat_id` (the tenant comes with the scope) and
/// `deleted_at IS NULL`.
fn live_messages(chat: &ChatRecord) -> Condition {
    live_messages_of(chat.id)
}

/// The live messages of the chat `chat_id`.
fn live_messages_of(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
}

fn message_row(model: message::Model) -> MessageRow {
    MessageRow {
        id: model.id,
        request_id: model.request_id,
        role: model.role,
        content: model.content,
        model: model.model,
        input_tokens: model.input_tokens,
        output_tokens: model.output_tokens,
        created_at: model.created_at,
    }
}

/// The chronological key of `messages.created_at` under the storage of the engine.
fn created_at_key(storage: TimeStorage) -> Expr {
    time_key(storage, Expr::col(message::Column::CreatedAt))
}

/// The bound of a `(created_at, id)` comparison: `at_or_before` selects the rows at or before
/// the position, `after` the rows strictly after it. `created_at` goes through the fixed-width
/// key on `SQLite` and the native type on `PostgreSQL`, with `id` as the tie-break.
fn position_condition(
    storage: TimeStorage,
    created_at: OffsetDateTime,
    id: Uuid,
    at_or_before: bool,
) -> Result<Condition, DomainError> {
    let bound: Value = time_bound(storage, created_at)
        .map_err(|e| DomainError::internal(format!("cannot bind the timestamp: {e}")))?;
    let key = created_at_key(storage);
    let (by_time, by_id) = if at_or_before {
        (key.clone().lt(bound.clone()), message::Column::Id.lte(id))
    } else {
        (key.clone().gt(bound.clone()), message::Column::Id.gt(id))
    };
    Ok(Condition::any()
        .add(by_time)
        .add(Condition::all().add(key.eq(bound)).add(by_id)))
}

/// The columns of an assistant message of a finalized turn.
#[derive(Debug, Clone)]
pub struct NewAssistantMessage {
    pub id: Uuid,
    pub request_id: Uuid,
    pub content: String,
    /// The effective model of the turn.
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub provider_response_id: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Message reads.
pub struct MessageRepo;

impl MessageRepo {
    /// One page of the live messages of the chat for a validated `OData` query: `deleted_at IS
    /// NULL` (compressed messages stay listed), `$filter` over `created_at`, `id` and `role`,
    /// default order `created_at asc` with `id` as the tie-break in the same direction, page
    /// size 20 by default and at most 100, keyset cursors forward and backward.
    ///
    /// # Errors
    ///
    /// [`DomainError::ODataQuery`] for a filter, order or cursor the query rejects; another
    /// [`DomainError`] when the database fails.
    pub async fn list_page(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        query: &ODataQuery,
        storage: TimeStorage,
    ) -> Result<Page<MessageRow>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-select-page
        // the tenant predicate (scope), `chat_id` from the scoped chat and `deleted_at IS NULL`
        // come before any filter or cursor position of the query
        let select = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(live_messages(chat));
        let page = message_page::paginate(runner, select, query, storage).await?;
        Ok(page.map_items(message_row))
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-select-page
    }

    /// The attachment batch of a page: `message_attachments` of the message ids joined to the
    /// attachments of the chat with `deleted_at IS NULL`, one batch per table.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn attachment_batch(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        message_ids: &[Uuid],
    ) -> Result<Vec<AttachmentRow>, DomainError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let scope = child_scope(chat);
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-attachments
        // the links of the page's messages in the chat; the tenant and `chat_id` of a link come
        // from the message row at the write
        let links = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat.id))
                    .add(message_attachment::Column::MessageId.is_in(message_ids.to_vec())),
            )
            .order_by(message_attachment::Column::CreatedAt, Order::Asc)
            .order_by(message_attachment::Column::AttachmentId, Order::Asc)
            .all(runner)
            .await?;
        if links.is_empty() {
            return Ok(Vec::new());
        }
        // the inner join: only the attachments of the chat that are not soft-deleted
        let attachment_ids: Vec<Uuid> = links.iter().map(|link| link.attachment_id).collect();
        let attachments = attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat.id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Id.is_in(attachment_ids)),
            )
            .all(runner)
            .await?;
        let by_id: BTreeMap<Uuid, attachment::Model> = attachments
            .into_iter()
            .map(|attachment| (attachment.id, attachment))
            .collect();
        let rows = links
            .into_iter()
            .filter_map(|link| {
                let attachment = by_id.get(&link.attachment_id)?;
                let thumbnail = match (
                    &attachment.img_thumbnail,
                    attachment.img_thumbnail_width,
                    attachment.img_thumbnail_height,
                ) {
                    (Some(data), Some(width), Some(height)) => Some(ThumbnailRow {
                        width,
                        height,
                        data: data.clone(),
                    }),
                    _ => None,
                };
                Some(AttachmentRow {
                    message_id: link.message_id,
                    attachment_id: attachment.id,
                    kind: attachment.attachment_kind.clone(),
                    filename: attachment.filename.clone(),
                    status: attachment.status.clone(),
                    thumbnail,
                })
            })
            .collect();
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-attachments
        Ok(rows)
    }

    /// The reactions of `user_id` on the message ids, one query.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn reaction_batch(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<BTreeMap<Uuid, Reaction>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-reactions
        // the caller's reactions only: the scope carries the tenant and the owner
        if message_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let scope = AccessScope::for_tenant(chat.tenant_id).ensure_owner(user_id);
        let rows = message_reaction::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.is_in(message_ids.to_vec())),
            )
            .all(runner)
            .await?;
        rows.into_iter()
            .map(|row| {
                let reaction = match row.reaction.as_str() {
                    "like" => Reaction::Like,
                    "dislike" => Reaction::Dislike,
                    other => {
                        return Err(DomainError::internal(format!(
                            "message {} has an unknown reaction `{other}`",
                            row.message_id
                        )));
                    }
                };
                Ok((row.message_id, reaction))
            })
            .collect()
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-projection:p1:inst-mp-reactions
    }

    /// The snapshot boundary: the latest non-deleted message of the chat in the order
    /// `(created_at, id)`, `None` for a chat without messages.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn snapshot_boundary(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        storage: TimeStorage,
    ) -> Result<Option<SnapshotBoundary>, DomainError> {
        Self::latest_message(runner, chat.tenant_id, chat.id, storage, None).await
    }

    /// The latest non-deleted message of the chat of the tenant in the order `(created_at, id)`,
    /// leaving out the messages of `excluded_request_id` when given; `None` when there is none.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn latest_message(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        storage: TimeStorage,
        excluded_request_id: Option<Uuid>,
    ) -> Result<Option<SnapshotBoundary>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-total-order
        // the latest non-deleted message in the strict total order `(created_at, id)`, with
        // `created_at` compared through the fixed-width key on SQLite
        let mut filter = live_messages_of(chat_id);
        if let Some(request_id) = excluded_request_id {
            // a message of no turn counts as a message of another turn
            filter = filter.add(
                Condition::any()
                    .add(message::Column::RequestId.is_null())
                    .add(message::Column::RequestId.ne(request_id)),
            );
        }
        let latest = message::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(filter)
            .order_by(created_at_key(storage), Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-total-order
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-return
        // the ordering contract: the boundary is the last message of the strict total order
        Ok(latest.into_iter().next().map(|row| SnapshotBoundary {
            created_at: row.created_at,
            id: row.id,
        }))
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-order:p1:inst-mo-return
    }

    /// The latest message of the chat in the order `(created_at, id)`, soft-deleted messages
    /// included; `None` for a chat without messages.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn latest_message_any(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        storage: TimeStorage,
    ) -> Result<Option<SnapshotBoundary>, DomainError> {
        let latest = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(Condition::all().add(message::Column::ChatId.eq(chat.id)))
            .order_by(created_at_key(storage), Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .all(runner)
            .await?;
        Ok(latest.into_iter().next().map(|row| SnapshotBoundary {
            created_at: row.created_at,
            id: row.id,
        }))
    }

    /// The live user message of the turn `request_id`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn user_message_of(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        let row = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                live_messages(chat)
                    .add(message::Column::RequestId.eq(request_id))
                    .add(message::Column::Role.eq("user")),
            )
            .one(runner)
            .await?;
        Ok(row.map(message_row))
    }

    /// Soft-delete every live message of the turn `request_id` (`deleted_at = now`).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn soft_delete_turn_messages(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-exclude-deleted
        Ok(message::Entity::update_many()
            .col_expr(message::Column::DeletedAt, Expr::value(Some(now)))
            .filter(live_messages(chat).add(message::Column::RequestId.eq(request_id)))
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?
            .rows_affected)
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order:p1:inst-mo-exclude-deleted
    }

    /// Clear `is_compressed` on every message of the chat.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn clear_compressed(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<(), DomainError> {
        message::Entity::update_many()
            .col_expr(message::Column::IsCompressed, Expr::value(false))
            .filter(Condition::all().add(message::Column::ChatId.eq(chat.id)))
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Take the row lock of the chat's thread summary with a write that changes nothing (the
    /// secure query API has no `FOR UPDATE`): a concurrent summary commit and a mutation are
    /// ordered by it on `PostgreSQL`. Returns the rows matched, 0 without a summary.
    ///
    /// # Errors
    ///
    /// A [`ScopeError`] when the database fails; the driver error is kept for the transaction
    /// retry helper.
    pub async fn lock_summary(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<u64, ScopeError> {
        Ok(thread_summary::Entity::update_many()
            .col_expr(
                thread_summary::Column::UpdatedAt,
                Expr::col(thread_summary::Column::UpdatedAt),
            )
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat.id)))
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?
            .rows_affected)
    }

    /// Delete the thread summary row of the chat.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn delete_summary(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<(), DomainError> {
        use toolkit_db::secure::SecureDeleteExt;
        thread_summary::Entity::delete_many()
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat.id)))
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// The attachments linked to the message `message_id`, with the original link time and the
    /// deleted flag of the attachment (the carry-forward read).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn linked_attachments(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        message_id: Uuid,
    ) -> Result<Vec<LinkedAttachment>, DomainError> {
        let scope = child_scope(chat);
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-select
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-only-original
        let links = message_attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(message_attachment::Column::ChatId.eq(chat.id))
                    .add(message_attachment::Column::MessageId.eq(message_id)),
            )
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-only-original
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-select
        if links.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = links.iter().map(|link| link.attachment_id).collect();
        let attachments = attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat.id))
                    .add(attachment::Column::Id.is_in(ids)),
            )
            .all(runner)
            .await?;
        let by_id: BTreeMap<Uuid, attachment::Model> = attachments
            .into_iter()
            .map(|attachment| (attachment.id, attachment))
            .collect();
        Ok(links
            .into_iter()
            .filter_map(|link| {
                let attachment = by_id.get(&link.attachment_id)?;
                Some(LinkedAttachment {
                    fact: AttachmentFact {
                        id: attachment.id,
                        kind: attachment.attachment_kind.clone(),
                        provider_file_id: attachment.provider_file_id.clone(),
                        secondary_file_id: attachment.secondary_file_id.clone(),
                        secondary_status: attachment.secondary_status.clone(),
                    },
                    link_created_at: link.created_at,
                    deleted: attachment.deleted_at.is_some(),
                })
            })
            .collect())
    }

    /// The recent messages of the context: at most `limit` messages with `request_id IS NOT
    /// NULL`, `deleted_at IS NULL`, `is_compressed = false`, `(created_at, id)` at most the
    /// boundary (when given) and, only when a summary exists, greater than its frontier; selected
    /// newest first and returned in chronological order.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails or a stored role is unknown.
    pub async fn recent_messages(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        boundary: Option<&SnapshotBoundary>,
        summary: Option<&ThreadSummaryRow>,
        limit: u32,
        storage: TimeStorage,
    ) -> Result<Vec<ContextMessage>, DomainError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-recent
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-sort-key
        // `request_id IS NOT NULL`, not deleted, not compressed, user and assistant messages
        // only, at most the snapshot boundary and, only with a summary, strictly after its
        // frontier; the timestamps are compared through the fixed-width key on SQLite and
        // natively on PostgreSQL, always with `id` as the tie-break
        let mut filter = live_messages(chat)
            .add(message::Column::RequestId.is_not_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(message::Column::Role.is_in(["user", "assistant"]));
        if let Some(boundary) = boundary {
            filter = filter.add(position_condition(
                storage,
                boundary.created_at,
                boundary.id,
                true,
            )?);
        }
        if let Some(summary) = summary {
            filter = filter.add(position_condition(
                storage,
                summary.frontier_created_at,
                summary.frontier_message_id,
                false,
            )?);
        }
        // newest first, then reversed to chronological order
        let mut rows = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(filter)
            .order_by(created_at_key(storage), Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(u64::from(limit))
            .all(runner)
            .await?;
        rows.reverse();
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-sort-key
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-recent
        rows.into_iter()
            .map(|row| {
                let role = match row.role.as_str() {
                    "user" => ContextRole::User,
                    "assistant" => ContextRole::Assistant,
                    other => {
                        return Err(DomainError::internal(format!(
                            "message {} has the unexpected role `{other}`",
                            row.id
                        )));
                    }
                };
                Ok(ContextMessage {
                    id: row.id,
                    role,
                    content: row.content,
                    created_at: row.created_at,
                })
            })
            .collect()
    }

    /// The thread summary of the chat, `None` without one.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn thread_summary(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<Option<ThreadSummaryRow>, DomainError> {
        Self::summary_of(runner, chat.tenant_id, chat.id).await
    }

    /// The thread summary of the chat of the tenant, `None` without one.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn summary_of(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRow>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-load-summary
        let row = thread_summary::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-load-summary
        Ok(row.map(|row| ThreadSummaryRow {
            summary_text: row.summary_text,
            token_estimate: row.token_estimate,
            frontier_created_at: row.summarized_up_to_created_at,
            frontier_message_id: row.summarized_up_to_message_id,
        }))
    }

    /// Insert the user message of a new turn into a resolved chat (`tenant_id` and `chat_id` from
    /// the chat): `role` `user`, `content_type` `text`, `token_estimate` 0, `request_kind`
    /// `chat`, `features_used` `[]`, token columns 0, `model` NULL, `is_compressed` false.
    ///
    /// # Errors
    ///
    /// [`DomainError::UniqueViolation`] when the `(chat_id, request_id, role)` index is
    /// violated; another [`DomainError`] when the database fails.
    pub async fn insert_user_message(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        id: Uuid,
        request_id: Uuid,
        content: &str,
        created_at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-user-message
        // `tenant_id` and `chat_id` come from the resolved chat, never from the request
        let row = message::ActiveModel {
            id: Set(id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(Some(request_id)),
            role: Set("user".to_owned()),
            content: Set(content.to_owned()),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(None),
            request_kind: Set("chat".to_owned()),
            features_used: Set(Json::Array(Vec::new())),
            input_tokens: Set(0),
            output_tokens: Set(0),
            cache_read_input_tokens: Set(0),
            cache_write_input_tokens: Set(0),
            reasoning_tokens: Set(0),
            model: Set(None),
            is_compressed: Set(false),
            created_at: Set(created_at),
            deleted_at: Set(None),
        };
        secure_insert::<message::Entity>(row, &child_scope(chat), runner).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-user-message
        Ok(())
    }

    /// Insert the assistant message of a finalized turn into the chat of the tenant (the ids come
    /// from the turn, never from input).
    ///
    /// # Errors
    ///
    /// [`DomainError::UniqueViolation`] when the `(chat_id, request_id, role)` index or the key is
    /// violated; another [`DomainError`] when the database fails.
    pub async fn insert_assistant_message(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        new: &NewAssistantMessage,
    ) -> Result<(), DomainError> {
        // the pre-allocated id, the turn's `request_id`, `role` `assistant`, the text as it is,
        // the effective model, the provider token counts (0 for a partial message), `content_type`
        // `text`, `request_kind` `chat`, `features_used` `[]`, `token_estimate` 0 and
        // `is_compressed` false
        let row = message::ActiveModel {
            id: Set(new.id),
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            request_id: Set(Some(new.request_id)),
            role: Set("assistant".to_owned()),
            content: Set(new.content.clone()),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(new.provider_response_id.clone()),
            request_kind: Set("chat".to_owned()),
            features_used: Set(Json::Array(Vec::new())),
            input_tokens: Set(new.input_tokens),
            output_tokens: Set(new.output_tokens),
            cache_read_input_tokens: Set(new.cache_read_input_tokens),
            cache_write_input_tokens: Set(new.cache_write_input_tokens),
            reasoning_tokens: Set(new.reasoning_tokens),
            model: Set(Some(new.model.clone())),
            is_compressed: Set(false),
            created_at: Set(new.created_at),
            deleted_at: Set(None),
        };
        secure_insert::<message::Entity>(row, &AccessScope::for_tenant(tenant_id), runner).await?;
        Ok(())
    }

    /// Link the attachments to the message (`tenant_id` and `chat_id` from the chat of the
    /// message row, never from input).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn link_attachments(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        message_id: Uuid,
        attachment_ids: &[Uuid],
        created_at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-links
        let rows: Vec<message_attachment::ActiveModel> = attachment_ids
            .iter()
            .map(|attachment_id| message_attachment::ActiveModel {
                tenant_id: Set(chat.tenant_id),
                chat_id: Set(chat.id),
                message_id: Set(message_id),
                attachment_id: Set(*attachment_id),
                created_at: Set(created_at),
            })
            .collect();
        toolkit_db::secure::secure_insert_many::<message_attachment::Entity>(
            rows,
            &child_scope(chat),
            runner,
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-links
        Ok(())
    }

    /// The non-deleted message `message_id` of the chat.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn find_message(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        message_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-load-message
        let row = message::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(live_messages(chat).add(message::Column::Id.eq(message_id)))
            .one(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-load-message
        Ok(row.map(message_row))
    }
}

#[cfg(test)]
#[path = "message_repo_tests.rs"]
mod message_repo_tests;
