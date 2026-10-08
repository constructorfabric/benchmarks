//! `messages` queries: counts for chat views, the paginated list, single lookups
//! and the context-assembly reads (recent messages, snapshot boundary, prior usage).

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DbErr, EntityTrait, ExprTrait, FromQueryResult,
    IntoActiveModel, QueryFilter, QueryOrder, QuerySelect,
};
use toolkit_db::odata::{FieldMap, LimitCfg, paginate_with_odata};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_odata::filter::FieldKind;
use toolkit_odata::{Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::model::MessageRole;
use crate::infra::db::entities::message::{self, Column, Entity};

/// Page size of `GET /v1/chats/{id}/messages` when `limit` is absent.
pub const DEFAULT_PAGE_LIMIT: u64 = 20;
/// Largest page size of the messages list; larger requests are clamped.
pub const MAX_PAGE_LIMIT: u64 = 100;

/// Tiebreaker of every message ordering.
const TIEBREAKER: (&str, SortDir) = ("id", SortDir::Asc);

/// `$filter` / `$orderby` fields of the messages list, each with a cursor extractor.
static MESSAGE_FIELDS: LazyLock<FieldMap<Entity>> = LazyLock::new(|| {
    FieldMap::new()
        .insert_with_extractor(
            "created_at",
            Column::CreatedAt,
            FieldKind::DateTimeUtc,
            |m: &message::Model| m.created_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )
        .insert_with_extractor("id", Column::Id, FieldKind::Uuid, |m: &message::Model| {
            m.id.to_string()
        })
        .insert_with_extractor(
            "role",
            Column::Role,
            FieldKind::String,
            |m: &message::Model| m.role.clone(),
        )
});

/// Position of a message in the deterministic order `(created_at, id)`.
pub type MessagePosition = (DateTime<Utc>, Uuid);

/// `(created_at, id) <= pos`, written out so it behaves identically on every backend.
fn at_or_before(pos: MessagePosition) -> Condition {
    Condition::any().add(Column::CreatedAt.lt(pos.0)).add(
        Condition::all()
            .add(Column::CreatedAt.eq(pos.0))
            .add(Column::Id.lte(pos.1)),
    )
}

/// `(created_at, id) > pos`.
fn after(pos: MessagePosition) -> Condition {
    Condition::any().add(Column::CreatedAt.gt(pos.0)).add(
        Condition::all()
            .add(Column::CreatedAt.eq(pos.0))
            .add(Column::Id.gt(pos.1)),
    )
}

#[derive(Debug, FromQueryResult)]
struct ChatCount {
    chat_id: Uuid,
    cnt: i64,
}

/// Queries over `messages`.
pub struct MessageRepo;

impl MessageRepo {
    /// Number of non-deleted messages of each chat in `chat_ids` (absent = 0).
    ///
    /// # Errors
    /// Database failures.
    pub async fn count_live_by_chat(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_ids: &[Uuid],
    ) -> DomainResult<HashMap<Uuid, i64>> {
        if chat_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.is_in(chat_ids.iter().copied()))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .project_all(runner, |q| {
                q.select_only()
                    .column(Column::ChatId)
                    .column_as(Expr::col(Column::Id).count(), "cnt")
                    .group_by(Column::ChatId)
                    .into_model::<ChatCount>()
            })
            .await?;
        Ok(rows.into_iter().map(|r| (r.chat_id, r.cnt)).collect())
    }

    /// Number of non-deleted messages of one chat.
    ///
    /// # Errors
    /// Database failures.
    pub async fn count_live(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<i64> {
        let counts = Self::count_live_by_chat(runner, tenant_id, &[chat_id]).await?;
        Ok(counts.get(&chat_id).copied().unwrap_or(0))
    }

    /// One page of the non-deleted messages of `chat_id` (default order
    /// `created_at asc`, tiebreaker `id asc`, limit 20, max 100).
    ///
    /// `created_at`, `id` and `role` are non-nullable, so the toolkit pager is used as is.
    ///
    /// # Errors
    /// `OData` errors (filter, order, cursor) and database failures.
    pub async fn list_page(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<message::Model>, ODataError> {
        let mut query = query.clone();
        if query.order.0.is_empty() && query.cursor.is_none() {
            query.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let select = Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .into_inner();
        paginate_with_odata::<Entity, _, _, _>(
            select,
            runner,
            &query,
            &MESSAGE_FIELDS,
            TIEBREAKER,
            LimitCfg {
                default: DEFAULT_PAGE_LIMIT,
                max: MAX_PAGE_LIMIT,
            },
            |m| m,
        )
        .await
    }

    /// The non-deleted message `id` of `chat_id`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_live(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> DomainResult<Option<message::Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::Id.eq(id))
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// The non-deleted user message of the turn `request_id` (retry/edit source).
    ///
    /// # Errors
    /// Database failures.
    pub async fn user_message_of_turn(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<Option<message::Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::RequestId.eq(request_id))
                    .add(Column::Role.eq(MessageRole::User.as_str()))
                    .add(Column::DeletedAt.is_null()),
            )
            .order_by_asc(Column::CreatedAt)
            .order_by_asc(Column::Id)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// Soft-delete the live messages (user and assistant) of the turn
    /// `request_id`; returns how many were deleted.
    ///
    /// # Errors
    /// Database failures.
    pub async fn soft_delete_turn_messages(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<u64> {
        let res = Entity::update_many()
            .col_expr(Column::DeletedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::RequestId.eq(request_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Insert `row` unless its id is already taken (`INSERT .. ON CONFLICT (id) DO
    /// NOTHING`: a taken id writes nothing instead of raising, so the caller's
    /// transaction stays usable on `PostgreSQL`). Returns `false` when nothing was
    /// inserted.
    ///
    /// # Errors
    /// Scope violations and database failures.
    pub async fn insert_if_absent(
        runner: &impl DBRunner,
        row: message::Model,
    ) -> DomainResult<bool> {
        let scope = AccessScope::for_tenant(row.tenant_id);
        let am = row.into_active_model().reset_all();
        match Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict_raw(OnConflict::column(Column::Id).do_nothing().to_owned())
            .exec(runner)
            .await
        {
            Ok(_) => Ok(true),
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The recent-messages query of DESIGN §4 "Recent messages query": live,
    /// non-compressed user/assistant messages with a `request_id`, at or before
    /// `boundary` and (when a thread summary exists) after its `frontier`, the
    /// newest `limit` of them.
    ///
    /// Rows come back **newest first** (`created_at DESC, id DESC`, as in the
    /// query); the caller reverses them to chronological order.
    ///
    /// # Errors
    /// Database failures.
    pub async fn recent_for_context(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: MessagePosition,
        frontier: Option<MessagePosition>,
        limit: u32,
    ) -> DomainResult<Vec<message::Model>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut cond = Condition::all()
            .add(Column::ChatId.eq(chat_id))
            .add(Column::RequestId.is_not_null())
            .add(Column::DeletedAt.is_null())
            .add(Column::IsCompressed.eq(false))
            .add(Column::Role.is_in([MessageRole::User.as_str(), MessageRole::Assistant.as_str()]))
            .add(at_or_before(boundary));
        if let Some(frontier) = frontier {
            cond = cond.add(after(frontier));
        }
        Ok(Entity::find()
            .filter(cond)
            .order_by_desc(Column::CreatedAt)
            .order_by_desc(Column::Id)
            .limit(u64::from(limit))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .all(runner)
            .await?)
    }

    /// Snapshot boundary: `(created_at, id)` of the latest non-deleted message of
    /// the chat, `None` for a chat without messages.
    ///
    /// # Errors
    /// Database failures.
    pub async fn snapshot_boundary(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<Option<MessagePosition>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .order_by_desc(Column::CreatedAt)
            .order_by_desc(Column::Id)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?
            .map(|m| (m.created_at, m.id)))
    }

    /// `prior_context_tokens` (DESIGN §5.4.1): `input_tokens + output_tokens` of the
    /// latest non-deleted assistant message whose token counts are not both zero;
    /// `0` when there is none.
    ///
    /// # Errors
    /// Database failures.
    pub async fn prior_context_tokens(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<i64> {
        let latest = Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::Role.eq(MessageRole::Assistant.as_str()))
                    .add(Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(Column::InputTokens.ne(0))
                            .add(Column::OutputTokens.ne(0)),
                    ),
            )
            .order_by_desc(Column::CreatedAt)
            .order_by_desc(Column::Id)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?;
        Ok(latest.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
    }

    /// Frozen target of a thread-summary task: `(created_at, id)` of the latest
    /// non-deleted message of the chat that does not belong to the turn
    /// `request_id`; `None` when there is none.
    ///
    /// # Errors
    /// Database failures.
    pub async fn latest_live_outside_turn(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<Option<MessagePosition>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(Column::RequestId.is_null())
                            .add(Column::RequestId.ne(request_id)),
                    ),
            )
            .order_by_desc(Column::CreatedAt)
            .order_by_desc(Column::Id)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?
            .map(|m| (m.created_at, m.id)))
    }

    /// Thread-summary range: the non-deleted, non-compressed messages of the
    /// chat in `(base, target]` (`base = None`: from the start), ordered
    /// `created_at ASC, id ASC`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn summary_range(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessagePosition>,
        target: MessagePosition,
    ) -> DomainResult<Vec<message::Model>> {
        let mut cond = Condition::all()
            .add(Column::ChatId.eq(chat_id))
            .add(Column::DeletedAt.is_null())
            .add(Column::IsCompressed.eq(false))
            .add(at_or_before(target));
        if let Some(base) = base {
            cond = cond.add(after(base));
        }
        Ok(Entity::find()
            .filter(cond)
            .order_by_asc(Column::CreatedAt)
            .order_by_asc(Column::Id)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .all(runner)
            .await?)
    }

    /// Mark the non-deleted messages of the chat in `(base, target]` compressed;
    /// returns how many rows changed.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_compressed(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        base: Option<MessagePosition>,
        target: MessagePosition,
    ) -> DomainResult<u64> {
        let mut cond = Condition::all()
            .add(Column::ChatId.eq(chat_id))
            .add(Column::DeletedAt.is_null())
            .add(Column::IsCompressed.eq(false))
            .add(at_or_before(target));
        if let Some(base) = base {
            cond = cond.add(after(base));
        }
        let res = Entity::update_many()
            .col_expr(Column::IsCompressed, Expr::value(true))
            .filter(cond)
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// The non-deleted message `id` of `chat_id`, read with `SELECT .. FOR
    /// UPDATE` (`PostgreSQL` only; `SQLite` has no row locks).
    ///
    /// # Errors
    /// Database failures.
    pub async fn lock_live(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> DomainResult<Option<message::Model>> {
        Ok(Entity::find()
            .filter(
                Condition::all()
                    .add(Column::Id.eq(id))
                    .add(Column::ChatId.eq(chat_id))
                    .add(Column::DeletedAt.is_null()),
            )
            .lock_exclusive()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod message_tests;
