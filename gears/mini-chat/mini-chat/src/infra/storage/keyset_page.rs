//! The shared keyset pager of the `OData` list pages (chats and messages): the platform cursor
//! format, validation and page info over the Secure ORM, with every timestamp compared, ordered
//! and paged through one chronological key.
//!
//! `SQLite` keeps the RFC 3339 text `sqlx` writes for an `OffsetDateTime`: its fraction has the
//! trailing zeros stripped and is omitted when zero (`...:00.12Z` after `...:00.123Z`, `...:00Z`
//! after `...:00.5Z`), so the text order is not the chronological one. Rather than depending on
//! how every writer formats the value, the `SQLite` comparisons go through [`text_time_key`], a
//! fixed-width form (`YYYY-MM-DDTHH:MM:SS.fffffffff`, nine fractional digits) computed from the
//! stored text in the `ORDER BY`, in every timestamp filter and in the keyset predicate, whose
//! bound values are formatted the same way ([`instant_key`]). The column keeps its RFC 3339 text
//! and `PostgreSQL` keeps the native comparison.
//!
//! A paged table implements [`PageSpec`]: its filterable fields, how each is ordered and put in
//! a cursor, and which of them are timestamps.

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{Condition, EntityTrait, Order, Value};
use time::OffsetDateTime;
use toolkit_db::odata::{
    FieldToColumn, LimitCfg, encode_cursor_value, filter_node_to_condition, parse_cursor_value,
};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::ast::{Expr as AstExpr, Value as AstValue};
use toolkit_odata::filter::{
    FieldKind, FilterField, FilterNode, FilterOp, ODataValue, convert_expr_to_filter_node,
};
use toolkit_odata::{
    CursorV1, Error as ODataError, ODataOrderBy, ODataQuery, Page, PageInfo, SortDir,
    validate_cursor_against,
};

use crate::domain::error::DomainError;

/// How the timestamps of a table are stored by the engine of the database, which decides how
/// they are compared, ordered and paged by cursor.
///
/// `PostgreSQL` keeps `timestamptz`, whose comparison is chronological. `SQLite` keeps RFC 3339
/// text (see the module documentation), which is compared through the fixed-width key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeStorage {
    /// `timestamptz` (`PostgreSQL`): columns and bound timestamps are compared as they are.
    Native,
    /// RFC 3339 text in UTC (`SQLite`): compared through the fixed-width key.
    Rfc3339Text,
}

impl TimeStorage {
    /// The storage of the engine named as [`toolkit_db`] reports it (`db_engine`).
    #[must_use]
    pub fn of_engine(engine: &str) -> Self {
        if engine == "sqlite" {
            Self::Rfc3339Text
        } else {
            Self::Native
        }
    }
}

/// What a paged table provides to the shared pager.
pub trait PageSpec {
    /// The filterable and orderable fields.
    type Field: FilterField;
    /// The paged entity.
    type Entity: EntityTrait;
    /// The column mapping used by the platform filter conversion.
    type Columns: FieldToColumn<Self::Field>;

    /// The page size of the list: the default and the clamp.
    const LIMITS: LimitCfg;

    /// The expression a field is ordered, compared and paged by: its column, or a non-null form
    /// of it (a nullable sort column needs a non-null key shared by `ORDER BY`, the cursor value
    /// and the keyset predicate).
    fn column(field: Self::Field) -> Expr;

    /// Whether the field is a timestamp column (compared through the fixed-width key on text
    /// storage).
    fn is_timestamp(field: Self::Field) -> bool;

    /// The value a model contributes to a cursor token for `field`.
    fn cursor_value(model: &<Self::Entity as EntityTrait>::Model, field: Self::Field) -> Value;
}

/// The fixed-width text key of an instant: UTC, nine fractional digits, no zone suffix, so the
/// text order of two keys is their chronological order.
fn instant_key(instant: OffsetDateTime) -> Result<String, String> {
    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]",
    )
    .map_err(|e| format!("invalid timestamp key format: {e}"))?;
    instant
        .to_offset(time::UtcOffset::UTC)
        .format(&format)
        .map_err(|e| format!("cannot format the timestamp: {e}"))
}

/// [`instant_key`] of a timestamp given as nanoseconds since the Unix epoch (`None` when the
/// `chrono` value does not fit).
fn nanos_key(nanos: Option<i64>) -> Result<String, String> {
    let nanos = nanos.ok_or_else(|| "timestamp out of range".to_owned())?;
    let instant = OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
        .map_err(|e| format!("timestamp out of range: {e}"))?;
    instant_key(instant)
}

/// The `SQLite` expression of the chronological key of a stored timestamp column: the stored
/// text is `YYYY-MM-DDTHH:MM:SS[.f{1,9}]Z`, so the seconds part (19 characters) is kept and the
/// fraction (what follows the `.`, without the `Z`) is right-padded with zeros to nine digits.
#[must_use]
pub fn text_time_key(column: Expr) -> Expr {
    Expr::cust_with_exprs(
        "substr(?, 1, 19) || '.' || substr(replace(substr(?, 21), 'Z', '') || '000000000', 1, 9)",
        [column.clone(), column],
    )
}

/// The expression a timestamp column is ordered and compared by: the fixed-width key on text
/// storage, the column itself on a native timestamp.
#[must_use]
pub fn time_key(storage: TimeStorage, column: Expr) -> Expr {
    match storage {
        TimeStorage::Rfc3339Text => text_time_key(column),
        TimeStorage::Native => column,
    }
}

/// The value an instant is compared to under [`time_key`].
///
/// # Errors
///
/// A description of the formatting failure.
pub fn time_bound(storage: TimeStorage, instant: OffsetDateTime) -> Result<Value, String> {
    match storage {
        TimeStorage::Rfc3339Text => instant_key(instant).map(Value::from),
        TimeStorage::Native => Ok(Value::from(instant)),
    }
}

/// The expression a field is ordered, compared and paged by.
fn sort_key<S: PageSpec>(storage: TimeStorage, field: S::Field) -> Expr {
    if storage == TimeStorage::Rfc3339Text && S::is_timestamp(field) {
        text_time_key(S::column(field))
    } else {
        S::column(field)
    }
}

/// The value a cursor token is compared to under [`sort_key`].
fn key_value<S: PageSpec>(
    storage: TimeStorage,
    field: S::Field,
    value: Value,
) -> Result<Value, String> {
    if storage != TimeStorage::Rfc3339Text || !S::is_timestamp(field) {
        return Ok(value);
    }
    let instant = match value {
        Value::TimeDateTimeWithTimeZone(Some(instant)) => instant,
        Value::ChronoDateTimeUtc(Some(instant)) => {
            return nanos_key(instant.timestamp_nanos_opt()).map(Value::from);
        }
        _ => return Err("not a timestamp".to_owned()),
    };
    instant_key(instant).map(Value::from)
}

/// Whether `expr` names a field of `F` typed as a UUID.
fn is_uuid_field<F: FilterField>(expr: &AstExpr) -> bool {
    matches!(expr, AstExpr::Identifier(name)
        if F::from_name(name).is_some_and(|field| matches!(field.kind(), FieldKind::Uuid)))
}

/// The literal `value` as a UUID when it is a string that parses as one (the quoted form
/// `id eq '<uuid>'`); any other value is returned as it is, so an invalid string stays a
/// type error of the platform conversion (`INVALID_FILTER`).
fn uuid_literal(value: &AstExpr) -> AstExpr {
    match value {
        AstExpr::Value(AstValue::String(text)) => uuid::Uuid::parse_str(text)
            .map_or_else(|_| value.clone(), |id| AstExpr::Value(AstValue::Uuid(id))),
        other => other.clone(),
    }
}

/// The filter with the quoted UUID literals of the UUID-typed fields of `F` turned into UUID
/// values, so that both `id eq <uuid>` and `id eq '<uuid>'` (the documented form of the
/// recovery call) are accepted.
fn accept_quoted_uuids<F: FilterField>(expr: &AstExpr) -> AstExpr {
    match expr {
        AstExpr::And(left, right) => AstExpr::And(
            Box::new(accept_quoted_uuids::<F>(left)),
            Box::new(accept_quoted_uuids::<F>(right)),
        ),
        AstExpr::Or(left, right) => AstExpr::Or(
            Box::new(accept_quoted_uuids::<F>(left)),
            Box::new(accept_quoted_uuids::<F>(right)),
        ),
        AstExpr::Not(inner) => AstExpr::Not(Box::new(accept_quoted_uuids::<F>(inner))),
        AstExpr::Compare(left, op, right) => {
            let (left, right) = if is_uuid_field::<F>(left) {
                (left.as_ref().clone(), uuid_literal(right))
            } else if is_uuid_field::<F>(right) {
                (uuid_literal(left), right.as_ref().clone())
            } else {
                (left.as_ref().clone(), right.as_ref().clone())
            };
            AstExpr::Compare(Box::new(left), *op, Box::new(right))
        }
        AstExpr::In(field, items) if is_uuid_field::<F>(field) => {
            AstExpr::In(field.clone(), items.iter().map(uuid_literal).collect())
        }
        other => other.clone(),
    }
}

/// The condition of a filter tree. Only the timestamp comparisons of the text storage are built
/// here (against the fixed-width key); everything else is the platform conversion.
fn filter_condition<S: PageSpec>(
    node: &FilterNode<S::Field>,
    storage: TimeStorage,
) -> Result<Condition, String> {
    let text = storage == TimeStorage::Rfc3339Text;
    match node {
        FilterNode::Binary {
            field,
            op,
            value: ODataValue::DateTime(instant),
        } if text && S::is_timestamp(*field) => {
            let key = Value::from(nanos_key(instant.timestamp_nanos_opt())?);
            let column = text_time_key(S::column(*field));
            let expr = match op {
                FilterOp::Eq => column.eq(key),
                FilterOp::Ne => column.ne(key),
                FilterOp::Gt => column.gt(key),
                FilterOp::Ge => column.gte(key),
                FilterOp::Lt => column.lt(key),
                FilterOp::Le => column.lte(key),
                other => return Err(format!("Unsupported operator for a timestamp: {other:?}")),
            };
            Ok(Condition::all().add(expr))
        }
        FilterNode::InList { field, values } if text && S::is_timestamp(*field) => {
            if values.is_empty() {
                return Err("IN list must not be empty".to_owned());
            }
            let keys = values
                .iter()
                .map(|value| match value {
                    ODataValue::DateTime(instant) => {
                        nanos_key(instant.timestamp_nanos_opt()).map(Value::from)
                    }
                    _ => Err("Expected a timestamp value".to_owned()),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Condition::all().add(text_time_key(S::column(*field)).is_in(keys)))
        }
        FilterNode::Composite { op, children } => {
            let base = match op {
                FilterOp::And => Condition::all(),
                FilterOp::Or => Condition::any(),
                _ => return Err(format!("Invalid composite operator: {op:?}")),
            };
            children.iter().try_fold(base, |acc, child| {
                Ok(acc.add(filter_condition::<S>(child, storage)?))
            })
        }
        FilterNode::Not(inner) => Ok(Condition::all()
            .add(filter_condition::<S>(inner, storage)?)
            .not()),
        other => filter_node_to_condition::<S::Field, S::Columns>(other),
    }
}

/// The keyset predicate of a cursor: the rows after (forward) or before (backward) its position
/// in the order, compared on the same keys the page is ordered by.
fn cursor_condition<S: PageSpec>(
    cursor: &CursorV1,
    order: &[(S::Field, SortDir)],
    storage: TimeStorage,
) -> Result<Condition, ODataError> {
    if cursor.k.len() != order.len() {
        return Err(ODataError::InvalidCursor);
    }
    let mut position = Vec::with_capacity(order.len());
    for (token, (field, dir)) in cursor.k.iter().zip(order) {
        let value =
            parse_cursor_value(field.kind(), token).map_err(|_| ODataError::InvalidCursor)?;
        let value =
            key_value::<S>(storage, *field, value).map_err(|_| ODataError::InvalidCursor)?;
        position.push((sort_key::<S>(storage, *field), value, *dir));
    }
    let backward = cursor.d == "bwd";
    let mut any = Condition::any();
    for (i, (key, value, dir)) in position.iter().enumerate() {
        let mut prefix = Condition::all();
        for (earlier, earlier_value, _) in position.iter().take(i) {
            prefix = prefix.add(earlier.clone().eq(earlier_value.clone()));
        }
        let comparison = match (dir, backward) {
            (SortDir::Asc, false) | (SortDir::Desc, true) => key.clone().gt(value.clone()),
            (SortDir::Desc, false) | (SortDir::Asc, true) => key.clone().lt(value.clone()),
        };
        any = any.add(prefix.add(comparison));
    }
    Ok(any)
}

/// The cursor token that continues after (`last`) or before the first row of `rows`.
fn cursor_token<S: PageSpec>(
    rows: &[<S::Entity as EntityTrait>::Model],
    order: &[(S::Field, SortDir)],
    signed_order: &ODataOrderBy,
    filter_hash: Option<&str>,
    direction: &str,
    last: bool,
) -> Result<Option<String>, ODataError> {
    let Some(row) = (if last { rows.last() } else { rows.first() }) else {
        return Ok(None);
    };
    let keys = order
        .iter()
        .map(|(field, _)| encode_cursor_value(&S::cursor_value(row, *field), field.kind()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ODataError::InvalidCursor)?;
    CursorV1 {
        k: keys,
        o: order.first().map_or(SortDir::Desc, |(_, dir)| *dir),
        s: signed_order.to_signed_tokens(),
        f: filter_hash.map(str::to_owned),
        d: direction.to_owned(),
    }
    .encode()
    .map(Some)
    .map_err(|_| ODataError::InvalidCursor)
}

/// The failure of the list query as a domain error: a rejected filter, order or cursor is a
/// query error (400); a database failure is internal.
fn odata_failure(error: ODataError) -> DomainError {
    match error {
        ODataError::Db(cause) => DomainError::internal(format!("list query failed: {cause}")),
        ODataError::ParsingUnavailable(cause) => {
            DomainError::internal(format!("list query unavailable: {cause}"))
        }
        other => DomainError::ODataQuery(other),
    }
}

/// The page of `select` for `query` (the keyset pagination of the platform `OData` pager:
/// same cursor format, same validation, same page info), ordered, filtered and compared on the
/// keys of [`sort_key`].
async fn read_page<S: PageSpec>(
    runner: &impl DBRunner,
    select: SecureSelect<S::Entity, Scoped>,
    query: &ODataQuery,
    effective: &ODataQuery,
    tiebreaker: SortDir,
    storage: TimeStorage,
) -> Result<Page<<S::Entity as EntityTrait>::Model>, ODataError> {
    let limit = query
        .limit
        .unwrap_or(S::LIMITS.default)
        .clamp(1, S::LIMITS.max);

    // the order of a continuation is the one its cursor carries
    let signed_order = match &query.cursor {
        Some(cursor) => {
            ODataOrderBy::from_signed_tokens(&cursor.s).map_err(|_| ODataError::InvalidCursor)?
        }
        None => effective.order.clone().ensure_tiebreaker("id", tiebreaker),
    };
    let order = signed_order
        .0
        .iter()
        .map(|key| {
            S::Field::from_name(&key.field)
                .map(|field| (field, key.dir))
                .ok_or_else(|| ODataError::InvalidOrderByField(key.field.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // a cursor issued for a filter carries only its hash, so a continuation must send the same
    // filter; a cursor without a hash is accepted
    if let Some(cursor) = &query.cursor
        && let Some(hash) = cursor.f.as_deref()
        && query.filter_hash.as_deref() != Some(hash)
    {
        return Err(ODataError::FilterMismatch);
    }

    let mut select = select;
    if let Some(ast) = query.filter.as_deref() {
        let ast = accept_quoted_uuids::<S::Field>(ast);
        let node = convert_expr_to_filter_node::<S::Field>(&ast)
            .map_err(|e| ODataError::InvalidFilter(e.to_string()))?;
        select = select
            .filter(filter_condition::<S>(&node, storage).map_err(ODataError::InvalidFilter)?);
    }

    let backward = query.cursor.as_ref().is_some_and(|c| c.d == "bwd");
    if let Some(cursor) = &query.cursor {
        select = select.filter(cursor_condition::<S>(cursor, &order, storage)?);
    }
    for (field, dir) in &order {
        let dir = match (dir, backward) {
            (SortDir::Asc, false) | (SortDir::Desc, true) => Order::Asc,
            (SortDir::Desc, false) | (SortDir::Asc, true) => Order::Desc,
        };
        select = select.order_by(sort_key::<S>(storage, *field), dir);
    }

    let mut rows = select
        .limit(limit + 1)
        .all(runner)
        .await
        .map_err(|e| ODataError::Db(e.to_string()))?;
    let has_more = (rows.len() as u64) > limit;
    if backward {
        if has_more {
            rows.pop();
        }
        rows.reverse();
    } else if has_more {
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }

    let hash = query.filter_hash.as_deref();
    let token = |direction: &str, last: bool| {
        cursor_token::<S>(&rows, &order, &signed_order, hash, direction, last)
    };
    let next_cursor = if backward || has_more {
        token("fwd", true)?
    } else {
        None
    };
    let prev_cursor = if backward {
        if has_more { token("bwd", false)? } else { None }
    } else if query.cursor.is_some() {
        token("bwd", false)?
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

/// One page of `select` (already scoped and restricted to the live rows) for `query`: the order
/// of the query, else `default_order`, made total by an `id` tie-break in the direction of its
/// first key; a continuation takes the order its cursor carries.
///
/// # Errors
///
/// [`DomainError::ODataQuery`] for an unknown filter or order field, an operator outside a
/// field's set, a malformed cursor or a cursor that does not match the order or the filter of
/// the query; `Internal` for a database failure.
pub async fn paginate<S: PageSpec>(
    runner: &impl DBRunner,
    select: SecureSelect<S::Entity, Scoped>,
    query: &ODataQuery,
    storage: TimeStorage,
    default_order: &ODataOrderBy,
) -> Result<Page<<S::Entity as EntityTrait>::Model>, DomainError> {
    let order = if query.order.0.is_empty() {
        default_order.clone()
    } else {
        query.order.clone()
    };
    let tiebreaker = order.0.first().map_or(SortDir::Desc, |key| key.dir);
    let effective = if query.cursor.is_none() {
        query.clone().with_order(order.clone())
    } else {
        query.clone()
    };
    // the cursor carries its own order: a query that names another one is a mismatch (the HTTP
    // extractor refuses `$orderby` together with a cursor before this point); a cursor that
    // cannot be decoded or was issued for another filter is refused while paginating
    if let Some(cursor) = &query.cursor
        && !query.order.0.is_empty()
    {
        let requested = order.clone().ensure_tiebreaker("id", tiebreaker);
        validate_cursor_against(cursor, &requested, query.filter_hash.as_deref())
            .map_err(odata_failure)?;
    }
    read_page::<S>(runner, select, query, &effective, tiebreaker, storage)
        .await
        .map_err(odata_failure)
}
