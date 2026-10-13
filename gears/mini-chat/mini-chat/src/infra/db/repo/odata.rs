//! `OData` field mappings of the list endpoints (DESIGN section 3.3 "List
//! Chats", "List Messages") and query normalization.
//!
//! - Chats: `updated_at` (date-time), `id` (UUID), `title` (string,
//!   nullable); default order `updated_at desc`, tiebreaker `id desc`. The
//!   title orders and pages by `COALESCE(title, '')` (an untitled chat sorts
//!   as the empty title on every backend), through
//!   [`super::paging::paginate_keyset`].
//! - Messages: `created_at` (date-time), `id` (UUID), `role` (string);
//!   default order `created_at asc`, tiebreaker `id asc`. The `id` filter
//!   also accepts a quoted UUID literal (`id eq '<uuid>'`), the form DESIGN
//!   "Turn Status API" documents for fetching an assistant message.
//!
//! Date-time filter literals and cursor values are bound in the
//! [`TimestampForm`] of the database (the mapping's [`TimestampBinding`]):
//! on `SQLite` they become the fixed-width text the timestamp columns store,
//! so text comparison is chronological. A date-time cursor key is then a
//! plain string to the cursor codec, so it is checked to be that fixed-width
//! text ([`is_timestamp_cursor_key`], [`check_timestamp_cursor`]): any other
//! key is `INVALID_CURSOR`, as on PostgreSQL.
//!
//! Pages default to 20 items and are clamped to 100.

use std::marker::PhantomData;

use sea_orm::Value;
use sea_orm::sea_query::{Expr, Func};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_db::odata::sea_orm_filter::PaginateOdataTryError;
use toolkit_db::odata::{FieldToColumn, LimitCfg, ODataFieldMapping};
use toolkit_odata::ast::{Expr as AstExpr, Value as AstValue};
use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, ODataValue};
use toolkit_odata::{Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use uuid::Uuid;

use super::paging::KeysetMapping;
use crate::domain::error::DomainError;
use crate::infra::db::entities::{TimestampForm, chats, fixed_text, messages};

// ── Timestamp binding ────────────────────────────────────────────────────

/// Type-level [`TimestampForm`] of a field mapping (the mapping hooks have
/// no access to the connection).
pub trait TimestampBinding: Send + Sync + 'static {
    const FORM: TimestampForm;
}

/// PostgreSQL: date-times bound as `TIMESTAMPTZ` values.
#[derive(Debug, Clone, Copy)]
pub struct NativeTimestamps;

impl TimestampBinding for NativeTimestamps {
    const FORM: TimestampForm = TimestampForm::Native;
}

/// `SQLite`: date-times bound as the fixed-width text of the columns.
#[derive(Debug, Clone, Copy)]
pub struct TextTimestamps;

impl TimestampBinding for TextTimestamps {
    const FORM: TimestampForm = TimestampForm::FixedText;
}

/// A date-time filter literal in `form` (other values unchanged).
fn timestamp_literal(form: TimestampForm, value: &ODataValue) -> Result<ODataValue, String> {
    match (form, value) {
        (TimestampForm::FixedText, ODataValue::DateTime(at)) => {
            let seconds = OffsetDateTime::from_unix_timestamp(at.timestamp())
                .map_err(|_| "date-time literal out of range".to_owned())?;
            let at = seconds
                .replace_nanosecond(at.timestamp_subsec_nanos())
                .map_err(|_| "date-time literal out of range".to_owned())?;
            Ok(ODataValue::String(fixed_text(at)))
        }
        _ => Ok(value.clone()),
    }
}

/// Cursor codec kind of a date-time order key in `form`.
const fn timestamp_cursor_kind(form: TimestampForm) -> FieldKind {
    match form {
        TimestampForm::Native => FieldKind::DateTimeUtc,
        TimestampForm::FixedText => FieldKind::String,
    }
}

/// Whether `key` is a well-formed cursor key of a date-time order key in
/// `form`. On [`TimestampForm::FixedText`] the codec decodes the key as a
/// plain string, so it must be the [`fixed_text`] of an RFC 3339 instant (the
/// only form the gear issues); on [`TimestampForm::Native`] the codec itself
/// parses the key as a date-time.
#[must_use]
pub fn is_timestamp_cursor_key(form: TimestampForm, key: &str) -> bool {
    match form {
        TimestampForm::Native => true,
        TimestampForm::FixedText => {
            OffsetDateTime::parse(key, &Rfc3339).is_ok_and(|at| fixed_text(at) == key)
        }
    }
}

/// Checks the cursor keys of the date-time order field `timestamp` in the
/// cursor of `query` (if any) before the toolkit paginator decodes them.
/// Order tokens and key counts are left to the paginator.
///
/// # Errors
///
/// [`ODataError::InvalidCursor`] when a key of `timestamp` is not a
/// well-formed date-time cursor key in `form`.
pub fn check_timestamp_cursor<F>(
    query: &ODataQuery,
    form: TimestampForm,
    timestamp: F,
) -> Result<(), ODataError>
where
    F: FilterField + PartialEq,
{
    let Some(cursor) = &query.cursor else {
        return Ok(());
    };
    let Ok(order) = ODataOrderBy::from_signed_tokens(&cursor.s) else {
        return Ok(());
    };
    let malformed = order.0.iter().zip(&cursor.k).any(|(key, value)| {
        F::from_name(&key.field) == Some(timestamp) && !is_timestamp_cursor_key(form, value)
    });
    if malformed {
        Err(ODataError::InvalidCursor)
    } else {
        Ok(())
    }
}

/// Page size limits of every list endpoint.
pub const LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

// ── Chats ────────────────────────────────────────────────────────────────

/// Filter and order fields of `GET /v1/chats`.
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

    fn nullable(&self) -> bool {
        matches!(self, Self::Title)
    }
}

/// `chats` columns of [`ChatField`], with timestamps bound as `T`.
#[derive(Debug, Clone, Copy)]
pub struct ChatODataMapping<T>(PhantomData<T>);

impl<T: TimestampBinding> FieldToColumn<ChatField> for ChatODataMapping<T> {
    type Column = chats::Column;

    fn map_field(field: ChatField) -> Self::Column {
        match field {
            ChatField::UpdatedAt => chats::Column::UpdatedAt,
            ChatField::Id => chats::Column::Id,
            ChatField::Title => chats::Column::Title,
        }
    }

    fn map_value(
        field: ChatField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            ChatField::UpdatedAt => timestamp_literal(T::FORM, value),
            ChatField::Id | ChatField::Title => Ok(value.clone()),
        }
    }
}

impl<T: TimestampBinding> ODataFieldMapping<ChatField> for ChatODataMapping<T> {
    type Entity = chats::Entity;

    fn extract_cursor_value(model: &chats::Model, field: ChatField) -> Value {
        match field {
            ChatField::UpdatedAt => T::FORM.value(model.updated_at.at()),
            ChatField::Id => Value::from(model.id),
            // The value of the `COALESCE(title, '')` sort key.
            ChatField::Title => Value::from(model.title.clone().unwrap_or_default()),
        }
    }

    fn cursor_kind(field: ChatField) -> FieldKind {
        match field {
            ChatField::UpdatedAt => timestamp_cursor_kind(T::FORM),
            ChatField::Id | ChatField::Title => field.kind(),
        }
    }
}

impl<T: TimestampBinding> KeysetMapping<ChatField> for ChatODataMapping<T> {
    fn accepts_cursor_key(field: ChatField, key: &str) -> bool {
        match field {
            ChatField::UpdatedAt => is_timestamp_cursor_key(T::FORM, key),
            ChatField::Id | ChatField::Title => true,
        }
    }

    /// The nullable title orders and compares as `COALESCE(title, '')`, so
    /// untitled chats take one well-defined place in both directions on every
    /// backend (raw NULLs sort first on `SQLite`, last on PostgreSQL, and
    /// never satisfy a keyset comparison).
    fn sort_key(field: ChatField) -> Expr {
        match field {
            ChatField::Title => {
                Func::coalesce([Expr::col(chats::Column::Title), Expr::val("")]).into()
            }
            ChatField::UpdatedAt | ChatField::Id => Expr::col(Self::map_field(field)),
        }
    }
}

/// Default order and tiebreaker of the chat list.
pub const CHAT_TIEBREAKER: (&str, SortDir) = ("id", SortDir::Desc);

// ── Messages ─────────────────────────────────────────────────────────────

/// Filter and order fields of `GET /v1/chats/{id}/messages`.
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

/// `messages` columns of [`MessageField`], with timestamps bound as `T`.
#[derive(Debug, Clone, Copy)]
pub struct MessageODataMapping<T>(PhantomData<T>);

impl<T: TimestampBinding> FieldToColumn<MessageField> for MessageODataMapping<T> {
    type Column = messages::Column;

    fn map_field(field: MessageField) -> Self::Column {
        match field {
            MessageField::CreatedAt => messages::Column::CreatedAt,
            MessageField::Id => messages::Column::Id,
            MessageField::Role => messages::Column::Role,
        }
    }

    fn map_value(
        field: MessageField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            MessageField::CreatedAt => timestamp_literal(T::FORM, value),
            MessageField::Id | MessageField::Role => Ok(value.clone()),
        }
    }
}

impl<T: TimestampBinding> ODataFieldMapping<MessageField> for MessageODataMapping<T> {
    type Entity = messages::Entity;

    fn extract_cursor_value(model: &messages::Model, field: MessageField) -> Value {
        match field {
            MessageField::CreatedAt => T::FORM.value(model.created_at.at()),
            MessageField::Id => Value::from(model.id),
            MessageField::Role => Value::from(model.role.clone()),
        }
    }

    fn cursor_kind(field: MessageField) -> FieldKind {
        match field {
            MessageField::CreatedAt => timestamp_cursor_kind(T::FORM),
            MessageField::Id | MessageField::Role => field.kind(),
        }
    }
}

/// Tiebreaker of the message list.
pub const MESSAGE_TIEBREAKER: (&str, SortDir) = ("id", SortDir::Asc);

// ── Query normalization ──────────────────────────────────────────────────

/// `query` with `default` as the order when it has neither an explicit order
/// nor a cursor (a cursor carries its own order).
pub fn with_default_order(query: &ODataQuery, default: (&str, SortDir)) -> ODataQuery {
    let mut query = query.clone();
    if query.cursor.is_none() && query.order.is_empty() {
        query.order = ODataOrderBy(vec![OrderKey {
            field: default.0.to_owned(),
            dir: default.1,
        }]);
    }
    query
}

/// `query` with every string literal compared to one of the UUID `fields`
/// (`eq`, `ne`, `in`) turned into a UUID literal when it parses as one. The
/// filter hash is kept, so cursors issued for either spelling stay valid for
/// the same request.
pub fn with_uuid_literals(query: &ODataQuery, fields: &[&str]) -> ODataQuery {
    let mut query = query.clone();
    if let Some(filter) = query.filter.take() {
        query.filter = Some(Box::new(coerce_uuid_literals(&filter, fields)));
    }
    query
}

fn is_uuid_field(expr: &AstExpr, fields: &[&str]) -> bool {
    match expr {
        AstExpr::Identifier(name) => {
            let last = name.rsplit('/').next().unwrap_or(name);
            fields.iter().any(|field| field.eq_ignore_ascii_case(last))
        }
        _ => false,
    }
}

fn coerce_literal(expr: &AstExpr) -> AstExpr {
    match expr {
        AstExpr::Value(AstValue::String(text)) => Uuid::parse_str(text.trim())
            .map_or_else(|_| expr.clone(), |id| AstExpr::Value(AstValue::Uuid(id))),
        other => other.clone(),
    }
}

fn coerce_uuid_literals(expr: &AstExpr, fields: &[&str]) -> AstExpr {
    match expr {
        AstExpr::And(left, right) => AstExpr::And(
            Box::new(coerce_uuid_literals(left, fields)),
            Box::new(coerce_uuid_literals(right, fields)),
        ),
        AstExpr::Or(left, right) => AstExpr::Or(
            Box::new(coerce_uuid_literals(left, fields)),
            Box::new(coerce_uuid_literals(right, fields)),
        ),
        AstExpr::Not(inner) => AstExpr::Not(Box::new(coerce_uuid_literals(inner, fields))),
        AstExpr::Compare(left, op, right) if is_uuid_field(left, fields) => {
            AstExpr::Compare(left.clone(), *op, Box::new(coerce_literal(right)))
        }
        AstExpr::In(left, items) if is_uuid_field(left, fields) => {
            AstExpr::In(left.clone(), items.iter().map(coerce_literal).collect())
        }
        other => other.clone(),
    }
}

/// Maps a fallible-pagination error into the domain error.
#[must_use]
pub fn paginate_error(err: PaginateOdataTryError<DomainError>) -> DomainError {
    match err {
        PaginateOdataTryError::OData(odata) => DomainError::InvalidQuery(odata),
        PaginateOdataTryError::MapError(domain) => domain,
    }
}

#[cfg(test)]
#[path = "odata_tests.rs"]
mod tests;
