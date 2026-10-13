//! `OData` field sets and column mappings of the paged lists.

use sea_orm::sea_query::{Expr, Func, SimpleExpr};
use sea_orm::{EntityTrait, IntoSimpleExpr, Value};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::filter::{FieldKind, FilterField, ODataValue};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};

use crate::domain::error::DomainError;
use crate::infra::db::entities::{chat, message};

/// Page size used when the request has no `limit`
/// (`cpt-cf-mini-chat-algo-chat-crud-paginate`).
const DEFAULT_LIMIT: u64 = 20;

/// Largest page size; a larger request is clamped, not rejected.
const MAX_LIMIT: u64 = 100;

/// Pages a scoped base query with the platform `OData` library
/// (`cpt-cf-mini-chat-algo-chat-crud-paginate`): `$filter` over the fields of
/// `F`, `$orderby` or the `default_order` field and direction, an `id`
/// tiebreaker in the direction of the first order key, keyset cursors and
/// `limit` + 1 rows to find the next page.
///
/// # Errors
///
/// [`DomainError::InvalidQuery`] for every `OData` failure.
pub async fn paginate_scoped<F, M, E, D>(
    select: SecureSelect<E, Scoped>,
    runner: &impl DBRunner,
    query: &ODataQuery,
    default_order: (&str, SortDir),
    to_domain: impl Fn(E::Model) -> D,
) -> Result<Page<D>, DomainError>
where
    F: FilterField,
    M: ODataFieldMapping<F, Entity = E>,
    E: EntityTrait,
{
    let mut query = query.clone();
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-compile-order
    let direction = if query.cursor.is_none() && query.order.0.is_empty() {
        query.order = ODataOrderBy(vec![OrderKey {
            field: default_order.0.to_owned(),
            dir: default_order.1,
        }]);
        default_order.1
    } else {
        query.order.0.first().map_or(default_order.1, |key| key.dir)
    };
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-compile-order

    // The platform library implements every step below: the effective limit
    // (default, clamp, `limit=0` rejection), `ORDER_WITH_CURSOR`, the filter
    // and order compilation, the cursor decode and match checks, the keyset
    // condition, the `limit + 1` select, the next and previous cursors and the
    // `OData` error reasons.
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-absent
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-default-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-zero
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-invalid-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-large
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-clamp-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-order-with-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-order-with-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-compile-filter
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-decode-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-check-cursor-match
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-apply-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-select-page
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-more-rows
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-set-next-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-else-last-page
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-null-next-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-set-prev-cursor
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-map-odata-errors
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-page
    paginate_odata::<F, M, E, D, _, _>(
        select,
        runner,
        &query,
        ("id", direction),
        LimitCfg {
            default: DEFAULT_LIMIT,
            max: MAX_LIMIT,
        },
        to_domain,
    )
    .await
    .map_err(DomainError::InvalidQuery)
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-page
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-map-odata-errors
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-set-prev-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-null-next-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-else-last-page
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-set-next-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-more-rows
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-select-page
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-apply-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-check-cursor-match
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-decode-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-compile-filter
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-order-with-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-order-with-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-clamp-limit
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-large
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-return-invalid-limit
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-zero
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-default-limit
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-paginate:p1:inst-if-limit-absent
}

/// The value a `$filter` date-time literal is bound as: the `time` value of the
/// stored `TIMESTAMPTZ` columns, the type the cursor path binds too. The
/// platform default (a `chrono` value) is encoded as different text on
/// `SQLite`, so `eq`, `gt` and `lt` against the stored rows would disagree.
/// `None` for any other literal (the platform conversion applies).
fn stored_datetime(value: &ODataValue) -> Option<Value> {
    let ODataValue::DateTime(at) = value else {
        return None;
    };
    let seconds = OffsetDateTime::from_unix_timestamp(at.timestamp()).ok()?;
    let at = seconds
        .replace_nanosecond(at.timestamp_subsec_nanos())
        .ok()?;
    Some(Value::TimeDateTimeWithTimeZone(Some(at)))
}

/// Fields of `GET /v1/chats` filter and order.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ChatFilterField {
    UpdatedAt,
    Id,
    Title,
}

impl FilterField for ChatFilterField {
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

/// Column mapping of the chat list.
pub struct ChatMapper;

impl FieldToColumn<ChatFilterField> for ChatMapper {
    type Column = chat::Column;

    fn map_field(field: ChatFilterField) -> chat::Column {
        match field {
            ChatFilterField::UpdatedAt => chat::Column::UpdatedAt,
            ChatFilterField::Id => chat::Column::Id,
            ChatFilterField::Title => chat::Column::Title,
        }
    }

    fn bind_value(field: ChatFilterField, value: &ODataValue) -> Option<Value> {
        match field {
            ChatFilterField::UpdatedAt => stored_datetime(value),
            ChatFilterField::Id | ChatFilterField::Title => None,
        }
    }

    /// `title` is nullable (an untitled chat) and `NULL` sorts first on
    /// `SQLite` but last on `PostgreSQL`; a keyset comparison against it skips
    /// the `NULL` rows. A stored title is never empty, so `COALESCE(title, '')`
    /// is a total order that puts the untitled chats first on both engines;
    /// the cursor carries the same coalesced value.
    fn sort_expr(field: ChatFilterField) -> SimpleExpr {
        match field {
            ChatFilterField::Title => {
                Func::coalesce([chat::Column::Title.into_simple_expr(), Expr::val("")]).into()
            }
            ChatFilterField::UpdatedAt | ChatFilterField::Id => {
                Self::map_field(field).into_simple_expr()
            }
        }
    }
}

impl ODataFieldMapping<ChatFilterField> for ChatMapper {
    type Entity = chat::Entity;

    fn extract_cursor_value(model: &chat::Model, field: ChatFilterField) -> Value {
        match field {
            ChatFilterField::UpdatedAt => Value::TimeDateTimeWithTimeZone(Some(model.updated_at)),
            ChatFilterField::Id => Value::Uuid(Some(model.id)),
            // The value of `sort_expr`: `COALESCE(title, '')`.
            ChatFilterField::Title => Value::String(Some(model.title.clone().unwrap_or_default())),
        }
    }
}

/// Fields of `GET /v1/chats/{id}/messages` filter and order.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MessageFilterField {
    CreatedAt,
    Id,
    Role,
}

impl FilterField for MessageFilterField {
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

/// Column mapping of the message list.
pub struct MessageMapper;

impl FieldToColumn<MessageFilterField> for MessageMapper {
    type Column = message::Column;

    fn map_field(field: MessageFilterField) -> message::Column {
        match field {
            MessageFilterField::CreatedAt => message::Column::CreatedAt,
            MessageFilterField::Id => message::Column::Id,
            MessageFilterField::Role => message::Column::Role,
        }
    }

    fn bind_value(field: MessageFilterField, value: &ODataValue) -> Option<Value> {
        match field {
            MessageFilterField::CreatedAt => stored_datetime(value),
            MessageFilterField::Id | MessageFilterField::Role => None,
        }
    }
}

impl ODataFieldMapping<MessageFilterField> for MessageMapper {
    type Entity = message::Entity;

    fn extract_cursor_value(model: &message::Model, field: MessageFilterField) -> Value {
        match field {
            MessageFilterField::CreatedAt => {
                Value::TimeDateTimeWithTimeZone(Some(model.created_at))
            }
            MessageFilterField::Id => Value::Uuid(Some(model.id)),
            MessageFilterField::Role => Value::String(Some(model.role.clone())),
        }
    }
}
