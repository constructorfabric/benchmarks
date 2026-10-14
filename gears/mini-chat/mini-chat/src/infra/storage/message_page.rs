//! The `OData` list page of messages (feature `cpt-cf-mini-chat-feature-message-streaming`,
//! algorithm `cpt-cf-mini-chat-algo-message-streaming-message-list-query`): the filterable and
//! orderable fields (`created_at`, `id`, `role`), the column mapping and the keyset pagination
//! over the Secure ORM on the shared pager of
//! [`keyset_page`](crate::infra::storage::keyset_page).
//!
//! The field set and the operators per field mirror the reference `OpenAPI` (`x-odata-filter`,
//! `x-odata-orderby`); the operator table is the platform's own (`FieldKind::allows`), so a
//! filter with an operator outside a field's set is refused as `INVALID_FILTER`.

use sea_orm::Value;
use sea_orm::sea_query::Expr;
use toolkit_db::odata::{FieldToColumn, LimitCfg};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};

use crate::domain::error::DomainError;
use crate::infra::storage::entity::message;
use crate::infra::storage::keyset_page::{self, PageSpec, TimeStorage};

// @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-limit
/// The page size of the message list: 20 when `limit` is absent, the value itself from 1 to 100
/// and 100 for anything larger (clamped, not an error). The extractor has already rejected `0`
/// (`INVALID_LIMIT`) and an unparsable value.
const PAGE_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};
// @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-limit

/// The filterable and orderable fields of the message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageField {
    const FIELDS: &'static [Self] = &[Self::CreatedAt, Self::Id, Self::Role];

    fn name(&self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Id => "id",
            Self::Role => "role",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::CreatedAt => FieldKind::DateTimeUtc,
            Self::Id => FieldKind::Uuid,
            Self::Role => FieldKind::String,
        }
    }
}

/// The column mapping of [`MessageField`] used by the platform filter conversion.
struct MessageColumns;

impl FieldToColumn<MessageField> for MessageColumns {
    type Column = message::Column;

    fn map_field(field: MessageField) -> message::Column {
        match field {
            MessageField::CreatedAt => message::Column::CreatedAt,
            MessageField::Id => message::Column::Id,
            MessageField::Role => message::Column::Role,
        }
    }
}

/// The message list as a paged table of the shared pager.
struct MessageSpec;

impl PageSpec for MessageSpec {
    type Field = MessageField;
    type Entity = message::Entity;
    type Columns = MessageColumns;

    const LIMITS: LimitCfg = PAGE_LIMITS;

    fn column(field: MessageField) -> Expr {
        Expr::col(MessageColumns::map_field(field))
    }

    fn is_timestamp(field: MessageField) -> bool {
        field == MessageField::CreatedAt
    }

    fn cursor_value(model: &message::Model, field: MessageField) -> Value {
        match field {
            MessageField::CreatedAt => Value::from(model.created_at),
            MessageField::Id => Value::from(model.id),
            MessageField::Role => Value::from(model.role.clone()),
        }
    }
}

/// One page of `select` (already scoped and restricted to the live messages of the chat) for
/// `query`.
///
/// # Errors
///
/// [`DomainError::ODataQuery`] for an unknown filter or order field, an operator outside a
/// field's set, a malformed cursor or a cursor that does not match the order or the filter of
/// the query; `Internal` for a database failure.
pub async fn paginate(
    runner: &impl DBRunner,
    select: SecureSelect<message::Entity, Scoped>,
    query: &ODataQuery,
    storage: TimeStorage,
) -> Result<Page<message::Model>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-default
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-default-order
    // neither `$orderby` nor a cursor: the chronological order `created_at asc`
    let default = ODataOrderBy(vec![OrderKey {
        field: MessageField::CreatedAt.name().to_owned(),
        dir: SortDir::Asc,
    }]);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-default-order
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-default
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-filter
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-orderby
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-tiebreak
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-sqlite-key
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-repo-errors
    // the filter is built from the typed field set (unknown field or operator outside the
    // field's set: `INVALID_FILTER`), the order is checked against the same set
    // (`INVALID_ORDERBY_FIELD`) and made total by `id` in the direction of its first key, the
    // cursor is opaque (`INVALID_CURSOR`, `ORDER_MISMATCH`, `FILTER_MISMATCH`) and serves next
    // and previous navigation, and `created_at` is filtered, ordered and paged through the
    // fixed-width key on text storage and natively on PostgreSQL; an error found while
    // paginating keeps the query error type of the extractor
    let page = keyset_page::paginate::<MessageSpec>(runner, select, query, storage, &default).await;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-repo-errors
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-sqlite-key
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-tiebreak
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-orderby
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-filter
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-return
    // the page info (effective limit, next and previous cursor) is built with the page
    page
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-return
}
