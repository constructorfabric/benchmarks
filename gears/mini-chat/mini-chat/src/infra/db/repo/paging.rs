//! Keyset pagination over expression sort keys (DESIGN section 3.3 "List
//! Chats").
//!
//! `toolkit_db::odata::paginate_odata` orders and compares the raw column of
//! every order key. That is wrong for a nullable key: NULL never satisfies the
//! keyset comparison, and NULLs sort first on `SQLite` but last on
//! PostgreSQL. [`paginate_keyset`] speaks the same cursor protocol
//! ([`CursorV1`] with signed order tokens, the filter hash and `fwd` / `bwd`
//! continuation, filters through the mapping's `map_value`) but orders and
//! compares every key through [`KeysetMapping::sort_key`], which a mapping
//! makes NULL-safe (e.g. `COALESCE(title, '')`).

use sea_orm::sea_query::{Expr, Order};
use sea_orm::{Condition, EntityTrait, ExprTrait, Value};
use toolkit_db::odata::{
    LimitCfg, ODataFieldMapping, encode_cursor_value, filter_node_to_condition, parse_cursor_value,
};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::filter::{FilterField, convert_expr_to_filter_node};
use toolkit_odata::{
    CursorV1, Error as ODataError, ODataOrderBy, ODataQuery, Page, PageInfo, SortDir,
};

/// A field mapping whose order keys are SQL expressions.
pub trait KeysetMapping<F: FilterField>: ODataFieldMapping<F> {
    /// The expression `field` is ordered and compared by. It must never be
    /// NULL for an orderable field, and
    /// [`ODataFieldMapping::extract_cursor_value`] must return its value.
    fn sort_key(field: F) -> Expr {
        Expr::col(Self::map_field(field))
    }

    /// Whether the raw cursor `key` of `field` is well-formed beyond what
    /// the cursor codec of [`ODataFieldMapping::cursor_kind`] checks (e.g. a
    /// date-time key carried as a plain string); a rejected key is
    /// [`ODataError::InvalidCursor`].
    fn accepts_cursor_key(field: F, key: &str) -> bool {
        let _ = (field, key);
        true
    }
}

/// Direction marker of a cursor continuing after the last item.
const FORWARD: &str = "fwd";
/// Direction marker of a cursor continuing before the first item.
const BACKWARD: &str = "bwd";

/// One page of `select` for `query`: `$filter`, `$orderby` (plus the
/// `tiebreaker`), `limit` clamped to `limits`, and cursor continuation in
/// both directions, with every order key ordered and compared through
/// [`KeysetMapping::sort_key`].
///
/// # Errors
///
/// The [`ODataError`] of an invalid filter, order field or cursor, a cursor
/// issued for another filter, or a failed query.
pub async fn paginate_keyset<F, M, D>(
    select: SecureSelect<M::Entity, Scoped>,
    runner: &impl DBRunner,
    query: &ODataQuery,
    tiebreaker: (&str, SortDir),
    limits: LimitCfg,
    to_domain: impl Fn(<M::Entity as EntityTrait>::Model) -> D,
) -> Result<Page<D>, ODataError>
where
    F: FilterField,
    M: KeysetMapping<F>,
{
    let limit = query.limit.unwrap_or(limits.default).clamp(1, limits.max);
    let order = match &query.cursor {
        Some(cursor) => {
            ODataOrderBy::from_signed_tokens(&cursor.s).map_err(|_| ODataError::InvalidCursor)?
        }
        None => query
            .order
            .clone()
            .ensure_tiebreaker(tiebreaker.0, tiebreaker.1),
    };
    let keys = order_keys::<F, M>(&order)?;
    if let Some(cursor) = &query.cursor
        && let Some(hash) = cursor.f.as_deref()
        && query.filter_hash.as_deref() != Some(hash)
    {
        return Err(ODataError::FilterMismatch);
    }

    let mut select = select;
    if let Some(filter) = query.filter.as_deref() {
        let node = convert_expr_to_filter_node::<F>(filter)
            .map_err(|err| ODataError::InvalidFilter(err.to_string()))?;
        let condition =
            filter_node_to_condition::<F, M>(&node).map_err(ODataError::InvalidFilter)?;
        select = select.filter(condition);
    }
    let backward = query.cursor.as_ref().is_some_and(|c| c.d == BACKWARD);
    if let Some(cursor) = &query.cursor {
        select = select.filter(beyond_cursor::<F, M>(cursor, &keys, backward)?);
    }
    for &(field, dir) in &keys {
        let ascending = (dir == SortDir::Asc) != backward;
        let sql_order = if ascending { Order::Asc } else { Order::Desc };
        select = select.order_by(M::sort_key(field), sql_order);
    }

    let mut rows = select
        .limit(limit.saturating_add(1))
        .all(runner)
        .await
        .map_err(|err| ODataError::Db(err.to_string()))?;
    let has_more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > limit;
    if has_more {
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    if backward {
        rows.reverse();
    }

    let cursor_from = |row: Option<&<M::Entity as EntityTrait>::Model>, direction: &str| {
        row.map(|row| cursor_token::<F, M>(row, &order, &keys, query, direction))
            .transpose()
    };
    let next_cursor = if backward || has_more {
        cursor_from(rows.last(), FORWARD)?
    } else {
        None
    };
    let prev_cursor = if (backward && has_more) || (!backward && query.cursor.is_some()) {
        cursor_from(rows.first(), BACKWARD)?
    } else {
        None
    };

    Ok(Page {
        items: rows.into_iter().map(to_domain).collect(),
        page_info: PageInfo {
            next_cursor,
            prev_cursor,
            limit,
        },
    })
}

/// The fields and directions of `order`, every one orderable.
fn order_keys<F, M>(order: &ODataOrderBy) -> Result<Vec<(F, SortDir)>, ODataError>
where
    F: FilterField,
    M: KeysetMapping<F>,
{
    order
        .0
        .iter()
        .map(|key| match F::from_name(&key.field) {
            Some(field) if M::is_orderable(field) => Ok((field, key.dir)),
            _ => Err(ODataError::InvalidOrderByField(key.field.clone())),
        })
        .collect()
}

/// The rows strictly after the cursor position in the scan direction: the
/// lexicographic keyset comparison over the sort keys.
fn beyond_cursor<F, M>(
    cursor: &CursorV1,
    keys: &[(F, SortDir)],
    backward: bool,
) -> Result<Condition, ODataError>
where
    F: FilterField,
    M: KeysetMapping<F>,
{
    if cursor.k.len() != keys.len() {
        return Err(ODataError::InvalidCursor);
    }
    let values = keys
        .iter()
        .zip(&cursor.k)
        .map(|(&(field, dir), token)| {
            if !M::accepts_cursor_key(field, token) {
                return Err(ODataError::InvalidCursor);
            }
            parse_cursor_value(M::cursor_kind(field), token)
                .map(|value| (field, dir, value))
                .map_err(|_| ODataError::InvalidCursor)
        })
        .collect::<Result<Vec<(F, SortDir, Value)>, _>>()?;

    let mut beyond = Condition::any();
    for (index, (field, dir, value)) in values.iter().enumerate() {
        let mut branch = Condition::all();
        for (equal_field, _, equal_value) in &values[..index] {
            branch = branch.add(M::sort_key(*equal_field).eq(equal_value.clone()));
        }
        let key = M::sort_key(*field);
        let after = if (*dir == SortDir::Asc) == backward {
            key.lt(value.clone())
        } else {
            key.gt(value.clone())
        };
        beyond = beyond.add(branch.add(after));
    }
    Ok(beyond)
}

/// The encoded cursor at `row` continuing in `direction`.
fn cursor_token<F, M>(
    row: &<M::Entity as EntityTrait>::Model,
    order: &ODataOrderBy,
    keys: &[(F, SortDir)],
    query: &ODataQuery,
    direction: &str,
) -> Result<String, ODataError>
where
    F: FilterField,
    M: KeysetMapping<F>,
{
    let k = keys
        .iter()
        .map(|&(field, _)| {
            encode_cursor_value(&M::extract_cursor_value(row, field), M::cursor_kind(field))
                .map_err(|_| ODataError::InvalidCursor)
        })
        .collect::<Result<Vec<_>, _>>()?;
    CursorV1 {
        k,
        o: keys.first().map_or(SortDir::Desc, |&(_, dir)| dir),
        s: order.to_signed_tokens(),
        f: query.filter_hash.clone(),
        d: direction.to_owned(),
    }
    .encode()
    .map_err(|_| ODataError::InvalidCursor)
}
