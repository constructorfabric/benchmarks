//! The `OData` list page of chats: the filterable and orderable fields (`updated_at`, `id`,
//! `title`), the column mapping and the keyset pagination over the Secure ORM.
//!
//! The field set and the operators per field mirror the reference `OpenAPI` (`x-odata-filter`,
//! `x-odata-orderby`); the operator table is the platform's own (`FieldKind::allows`), so a
//! filter with an operator outside a field's set is refused as `INVALID_FILTER`. The pager
//! itself (cursors, keys, timestamp comparison) is the shared one of
//! [`keyset_page`](crate::infra::storage::keyset_page).

use sea_orm::Value;
use sea_orm::sea_query::{Expr, Func};
use toolkit_db::odata::{FieldToColumn, LimitCfg};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};

use crate::domain::error::DomainError;
use crate::infra::storage::entity::chat;
use crate::infra::storage::keyset_page::{self, PageSpec};

pub use crate::infra::storage::keyset_page::TimeStorage;

// @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit
// @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-absent
// @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-valid
// @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-clamp
/// The page size of the chat list: 20 when `limit` is absent, the value itself from 1 to 100 and
/// 100 for anything larger (clamped, not an error). The extractor has already rejected `0` and
/// an unparsable value.
const PAGE_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};
// @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-clamp
// @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-valid
// @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-absent
// @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit

/// The filterable and orderable fields of the chat list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatField {
    const FIELDS: &'static [Self] = &[Self::UpdatedAt, Self::Id, Self::Title];

    fn name(&self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated_at",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::UpdatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Title => FieldKind::String,
        }
    }
}

/// The column mapping of [`ChatField`] used by the platform filter conversion.
struct ChatColumns;

impl FieldToColumn<ChatField> for ChatColumns {
    type Column = chat::Column;

    fn map_field(field: ChatField) -> chat::Column {
        match field {
            ChatField::UpdatedAt => chat::Column::UpdatedAt,
            ChatField::Id => chat::Column::Id,
            ChatField::Title => chat::Column::Title,
        }
    }
}

/// The chat list as a paged table of the shared pager.
struct ChatSpec;

impl PageSpec for ChatSpec {
    type Field = ChatField;
    type Entity = chat::Entity;
    type Columns = ChatColumns;

    const LIMITS: LimitCfg = PAGE_LIMITS;

    /// The title of an untitled chat (`NULL`) is the empty title, the same value the cursor
    /// carries, so that `ORDER BY`, the keyset predicate and the cursor agree on a non-null key
    /// (a `NULL` key makes every keyset comparison unknown, and `NULL`s sort first on `SQLite`
    /// and last on `PostgreSQL`); the stored column stays `NULL`.
    fn column(field: ChatField) -> Expr {
        match field {
            ChatField::UpdatedAt => Expr::col(chat::Column::UpdatedAt),
            ChatField::Id => Expr::col(chat::Column::Id),
            ChatField::Title => {
                Func::coalesce([Expr::col(chat::Column::Title), Expr::val("")]).into()
            }
        }
    }

    fn is_timestamp(field: ChatField) -> bool {
        field == ChatField::UpdatedAt
    }

    /// The value a model contributes to a cursor token for `field`.
    fn cursor_value(model: &chat::Model, field: ChatField) -> Value {
        match field {
            ChatField::UpdatedAt => Value::from(model.updated_at),
            ChatField::Id => Value::from(model.id),
            // an untitled chat sorts under the empty title
            ChatField::Title => Value::from(model.title.clone().unwrap_or_default()),
        }
    }
}

/// The order of a query: the client's order, else `updated_at desc`.
fn default_order() -> ODataOrderBy {
    ODataOrderBy(vec![OrderKey {
        field: ChatField::UpdatedAt.name().to_owned(),
        dir: SortDir::Desc,
    }])
}

/// One page of `select` (already scoped and restricted to live chats) for `query`.
///
/// # Errors
///
/// [`DomainError::ODataQuery`] for an unknown filter or order field, an operator outside a
/// field's set, a malformed cursor or a cursor that does not match the order or the filter of
/// the query; `Internal` for a database failure.
pub async fn paginate(
    runner: &impl DBRunner,
    select: SecureSelect<chat::Entity, Scoped>,
    query: &ODataQuery,
    storage: TimeStorage,
) -> Result<Page<chat::Model>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-default-order
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-apply-default-order
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-list-order
    // neither `$orderby` nor a cursor: the most recently active chat first, with `id` as the
    // tiebreaker in the direction of the primary key (served by the partial index on
    // `(tenant_id, user_id, updated_at DESC)`)
    let default = default_order();
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-list-order
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-apply-default-order
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-default-order

    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-filter
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-orderby
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-validate-query
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-query-invalid
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-400
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-select
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-cursor-failed
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-cursor-error
    // the cursor carries its own order: a query that names another one is a mismatch (the HTTP
    // extractor refuses `$orderby` together with a cursor before this point); a cursor that
    // cannot be decoded or was issued for another filter is refused while paginating. The
    // filter is built from the typed field set (unknown field or operator outside the field's
    // set: `INVALID_FILTER`), the order is checked against the same set
    // (`INVALID_ORDERBY_FIELD`), and the page is read after the cursor position
    let page = keyset_page::paginate::<ChatSpec>(runner, select, query, storage, &default).await;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-cursor-error
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-cursor-failed
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-select
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return-400
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-query-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-validate-query
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-orderby
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-filter
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return
    // the page info (effective limit, next and previous cursor) is built with the page
    page
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return
}
