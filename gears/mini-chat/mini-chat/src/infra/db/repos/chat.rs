//! `chats` queries. Every user-path method takes the caller's chat scope
//! (tenant + owner); soft-deleted chats are invisible to all of them.

use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, ExprTrait, Order, QueryFilter, QueryOrder,
};
use toolkit_db::odata::{FieldMap, build_cursor_for_model, expr_to_condition};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::filter::FieldKind;
use toolkit_odata::{
    CursorV1, Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, Page, PageInfo, SortDir,
};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::infra::db::entities::{attachment, chat};

/// Page size of `GET /v1/chats` when `limit` is absent.
pub const DEFAULT_PAGE_LIMIT: u64 = 20;
/// Largest page size of `GET /v1/chats`; larger requests are clamped.
pub const MAX_PAGE_LIMIT: u64 = 100;

/// `$filter` / `$orderby` fields of `GET /v1/chats`, each with a cursor extractor.
static CHAT_FIELDS: LazyLock<FieldMap<chat::Entity>> = LazyLock::new(|| {
    FieldMap::new()
        .insert_with_extractor(
            "updated_at",
            chat::Column::UpdatedAt,
            FieldKind::DateTimeUtc,
            |m: &chat::Model| m.updated_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )
        .insert_with_extractor(
            "id",
            chat::Column::Id,
            FieldKind::Uuid,
            |m: &chat::Model| m.id.to_string(),
        )
        .insert_with_extractor(
            "title",
            chat::Column::Title,
            FieldKind::String,
            |m: &chat::Model| m.title.clone().unwrap_or_default(),
        )
});

/// Fields of a new chat row.
#[derive(Debug, Clone)]
pub struct NewChat {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub now: DateTime<Utc>,
}

fn live(id: Uuid) -> Condition {
    Condition::all()
        .add(chat::Column::Id.eq(id))
        .add(chat::Column::DeletedAt.is_null())
}

/// Queries over `chats`.
pub struct ChatRepo;

impl ChatRepo {
    /// Insert a chat (`created_at = updated_at = now`), validated against `scope`.
    ///
    /// # Errors
    /// Scope violations and database failures.
    pub async fn insert(
        runner: &impl DBRunner,
        scope: &AccessScope,
        new: NewChat,
    ) -> DomainResult<chat::Model> {
        let am = chat::ActiveModel {
            id: ActiveValue::Set(new.id),
            tenant_id: ActiveValue::Set(new.tenant_id),
            user_id: ActiveValue::Set(new.user_id),
            model: ActiveValue::Set(Some(new.model)),
            title: ActiveValue::Set(new.title),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(new.now),
            updated_at: ActiveValue::Set(new.now),
            deleted_at: ActiveValue::Set(None),
        };
        Ok(secure_insert::<chat::Entity>(am, scope, runner).await?)
    }

    /// The non-deleted chat `id` visible in `scope`.
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_live(
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> DomainResult<Option<chat::Model>> {
        Ok(chat::Entity::find()
            .filter(live(id))
            .secure()
            .scope_with(scope)
            .one(runner)
            .await?)
    }

    /// The chat `id` of `tenant_id`, soft-deleted or not (outbox cleanup handlers).
    ///
    /// # Errors
    /// Database failures.
    pub async fn find_any(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
    ) -> DomainResult<Option<chat::Model>> {
        Ok(chat::Entity::find()
            .filter(chat::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// Set the title and `updated_at`; `None` when the chat is not visible.
    ///
    /// # Errors
    /// Database failures.
    pub async fn update_title(
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        title: String,
        now: DateTime<Utc>,
    ) -> DomainResult<Option<chat::Model>> {
        let rows = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live(id))
            .secure()
            .scope_with(scope)
            .exec_with_returning(runner)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Bump `updated_at` of a live chat (send, retry and edit transactions).
    ///
    /// # Errors
    /// Database failures.
    pub async fn touch(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<()> {
        chat::Entity::update_many()
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Take the write lock for the rest of the caller's transaction with a write
    /// that changes nothing (`UPDATE chats SET updated_at = updated_at WHERE id =
    /// ? AND tenant_id = ?`). It is the transaction's first statement: `SQLite`
    /// then acquires the database write lock up front, waiting under
    /// `busy_timeout`, instead of upgrading a read snapshot that a concurrent
    /// commit made stale (`SQLITE_BUSY_SNAPSHOT`, not waited for). On
    /// `PostgreSQL` it row-locks the chat, ordering transactions of one chat.
    ///
    /// # Errors
    /// Database failures.
    pub async fn lock_for_write(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        id: Uuid,
    ) -> DomainResult<()> {
        chat::Entity::update_many()
            .col_expr(
                chat::Column::UpdatedAt,
                Expr::col((chat::Entity, chat::Column::UpdatedAt)),
            )
            .filter(chat::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Soft-delete the chat (`deleted_at = updated_at = now`); `None` when the chat
    /// is not visible or already deleted.
    ///
    /// # Errors
    /// Database failures.
    pub async fn soft_delete(
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<Option<chat::Model>> {
        let rows = chat::Entity::update_many()
            .col_expr(chat::Column::DeletedAt, Expr::value(now))
            .col_expr(chat::Column::UpdatedAt, Expr::value(now))
            .filter(live(id))
            .secure()
            .scope_with(scope)
            .exec_with_returning(runner)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Hand the chat's non-deleted attachments without a cleanup state to the
    /// chat-deletion cleanup (`cleanup_status = 'pending'`). Returns the row count.
    ///
    /// # Errors
    /// Database failures.
    pub async fn mark_attachments_cleanup_pending(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        now: DateTime<Utc>,
    ) -> DomainResult<u64> {
        let res = attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// One page of the non-deleted chats visible in `scope` (default order
    /// `updated_at desc`, tiebreaker `id desc`, limit 20, max 100).
    ///
    /// Same `OData` semantics and cursor format as the toolkit's
    /// `paginate_with_odata`, except that `title` is ordered and compared as
    /// `COALESCE(title, '')`, so untitled chats (NULL title, cursor key `""`)
    /// stay reachable when paging by title.
    ///
    /// # Errors
    /// `OData` errors (filter, order, cursor) and database failures.
    pub async fn list_page(
        runner: &impl DBRunner,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<chat::Model>, ODataError> {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
        let order = effective_order(query)?;
        if let Some(cur) = &query.cursor
            && let Some(hash) = cur.f.as_deref()
            && query.filter_hash.as_deref() != Some(hash)
        {
            return Err(ODataError::FilterMismatch);
        }

        let mut select = chat::Entity::find().filter(chat::Column::DeletedAt.is_null());
        if let Some(ast) = query.filter.as_deref() {
            let cond = expr_to_condition::<chat::Entity>(ast, &CHAT_FIELDS)
                .map_err(|e| ODataError::InvalidFilter(e.to_string()))?;
            select = select.filter(cond);
        }
        let backward = query.cursor.as_ref().is_some_and(|c| c.d == "bwd");
        if let Some(cur) = &query.cursor {
            select = select.filter(cursor_condition(cur, &order, backward)?);
        }
        let query_order = if backward {
            order.clone().reverse_directions()
        } else {
            order.clone()
        };
        for key in &query_order.0 {
            let dir = match key.dir {
                SortDir::Asc => Order::Asc,
                SortDir::Desc => Order::Desc,
            };
            select = select.order_by(sort_expr(&key.field)?, dir);
        }

        let mut rows = select
            .secure()
            .scope_with(scope)
            .limit(limit + 1)
            .all(runner)
            .await
            .map_err(|e| ODataError::Db(e.to_string()))?;
        let has_more = rows.len() as u64 > limit;
        if has_more {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        if backward {
            rows.reverse();
        }

        let cursor = |row: Option<&chat::Model>, direction: &str| {
            row.map(|m| {
                build_cursor_for_model::<chat::Entity>(
                    m,
                    &order,
                    &CHAT_FIELDS,
                    TIEBREAKER.1,
                    query.filter_hash.clone(),
                    direction,
                )
                .and_then(|c| c.encode().map_err(|_| ODataError::InvalidCursor))
            })
            .transpose()
        };
        let next_cursor = if backward || has_more {
            cursor(rows.last(), "fwd")?
        } else {
            None
        };
        let prev_cursor = if (backward && has_more) || (!backward && query.cursor.is_some()) {
            cursor(rows.first(), "bwd")?
        } else {
            None
        };
        Ok(Page {
            items: rows,
            page_info: PageInfo {
                next_cursor,
                prev_cursor,
                limit,
            },
        })
    }
}

/// Tiebreaker of every chat ordering.
const TIEBREAKER: (&str, SortDir) = ("id", SortDir::Desc);

/// Order from the cursor (it carries its own), else the client's `$orderby`
/// (default `updated_at desc`), plus the `id desc` tiebreaker.
fn effective_order(query: &ODataQuery) -> Result<ODataOrderBy, ODataError> {
    if let Some(cur) = &query.cursor {
        return ODataOrderBy::from_signed_tokens(&cur.s).map_err(|_| ODataError::InvalidCursor);
    }
    let order = if query.order.0.is_empty() {
        ODataOrderBy(vec![OrderKey {
            field: "updated_at".to_owned(),
            dir: SortDir::Desc,
        }])
    } else {
        query.order.clone()
    };
    Ok(order.ensure_tiebreaker(TIEBREAKER.0, TIEBREAKER.1))
}

/// Sort/compare expression of an orderable field (`title` is NULL-safe).
fn sort_expr(field: &str) -> Result<Expr, ODataError> {
    let f = CHAT_FIELDS
        .get(field)
        .ok_or_else(|| ODataError::InvalidOrderByField(field.to_owned()))?;
    Ok(if field.eq_ignore_ascii_case("title") {
        Func::coalesce([Expr::col(chat::Column::Title), Expr::val("")]).into()
    } else {
        Expr::col(f.col)
    })
}

fn cursor_value(kind: FieldKind, raw: &str) -> Result<sea_orm::Value, ODataError> {
    let value = match kind {
        FieldKind::String => Some(sea_orm::Value::String(Some(raw.to_owned()))),
        FieldKind::Uuid => Uuid::parse_str(raw)
            .ok()
            .map(|u| sea_orm::Value::Uuid(Some(u))),
        FieldKind::DateTimeUtc => DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|d| sea_orm::Value::ChronoDateTimeUtc(Some(d.with_timezone(&Utc)))),
        _ => None,
    };
    value.ok_or(ODataError::InvalidCursor)
}

/// Keyset predicate after (`fwd`) or before (`bwd`) the cursor row:
/// `(k0 > v0) OR (k0 = v0 AND k1 > v1) OR ...` with `<` for descending keys.
fn cursor_condition(
    cursor: &CursorV1,
    order: &ODataOrderBy,
    backward: bool,
) -> Result<Condition, ODataError> {
    if cursor.k.len() != order.0.len() {
        return Err(ODataError::InvalidCursor);
    }
    let mut keys = Vec::with_capacity(order.0.len());
    for (key, raw) in order.0.iter().zip(&cursor.k) {
        let field = CHAT_FIELDS
            .get(&key.field)
            .ok_or(ODataError::InvalidCursor)?;
        let expr = sort_expr(&key.field).map_err(|_| ODataError::InvalidCursor)?;
        keys.push((expr, cursor_value(field.kind, raw)?, key.dir));
    }
    let mut any = Condition::any();
    for (i, (expr, value, dir)) in keys.iter().enumerate() {
        let mut all = Condition::all();
        for (prev, prev_value, _) in &keys[..i] {
            all = all.add(prev.clone().eq(prev_value.clone()));
        }
        let ascending = matches!(dir, SortDir::Asc) != backward;
        all = all.add(if ascending {
            expr.clone().gt(value.clone())
        } else {
            expr.clone().lt(value.clone())
        });
        any = any.add(all);
    }
    Ok(any)
}
