//! Keyset (cursor) pagination over a secure select, shared by the chat and message lists.
//!
//! The platform paginator (`toolkit_db::odata::paginate_odata`) is not used because it binds the
//! parser's `chrono` literals (see [`super::odata_time`]) and cannot page by a nullable column.
//! This is the same keyset pagination (platform cursor format, `limit + 1` probing, `fwd` /
//! `bwd` cursors, clamped limit) over a per-field sort expression ([`KeysetField::sort_expr`])
//! and cursor value ([`KeysetRow::sort_value`]); datetime `$filter` literals are bound as stored
//! values by [`filter_condition`].

use sea_orm::sea_query::{Expr, ExprTrait as _};
use sea_orm::{Condition, EntityTrait, Order};
use toolkit_db::odata::{FieldToColumn, LimitCfg, encode_cursor_value, parse_cursor_value};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::ast::{Expr as FilterExpr, Value};
use toolkit_odata::filter::{FieldKind, FilterField, convert_expr_to_filter_node};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page, PageInfo, SortDir};

use super::odata_time::filter_condition;
use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::ts;

/// A list field: how it sorts in SQL.
pub trait KeysetField: FilterField + Copy {
    /// The column expression the list is ordered and keyed by.
    fn sort_expr(self) -> Expr;
}

/// A row of the list: the value of each field as carried in a cursor (it must equal
/// [`KeysetField::sort_expr`] evaluated on the row).
pub trait KeysetRow {
    type Field: KeysetField;

    fn sort_value(&self, field: Self::Field) -> sea_orm::Value;
}

/// What distinguishes one list from another.
pub struct KeysetSpec<'a, F> {
    /// Order when the query has neither `$orderby` nor a cursor.
    pub default_order: (F, SortDir),
    /// Appended to every order that does not end in it, so the order is total.
    pub tiebreaker: (F, SortDir),
    /// Datetime fields of `$filter` (bound as stored values).
    pub time_fields: &'a [F],
    pub limits: LimitCfg,
}

/// The filter with every quoted UUID literal compared with a `Uuid` field turned into a UUID
/// value. The platform parser only builds UUID values from the unquoted form
/// (`id eq 0b9c...`), while DESIGN tells clients to filter by `id eq '<uuid>'`; both are
/// accepted. A quoted string that is not a UUID stays a string, which the filter conversion
/// refuses as a type mismatch (400 `INVALID_FILTER`). The filter hash of cursors is computed
/// from the original expression, so cursors are unaffected.
fn accept_quoted_uuids<F: FilterField>(expr: &FilterExpr) -> FilterExpr {
    fn literal(value: &FilterExpr) -> FilterExpr {
        match value {
            FilterExpr::Value(Value::String(text)) => text
                .parse()
                .map_or_else(|_| value.clone(), |id| FilterExpr::Value(Value::Uuid(id))),
            other => other.clone(),
        }
    }
    fn is_uuid_field<F: FilterField>(expr: &FilterExpr) -> bool {
        matches!(expr, FilterExpr::Identifier(name)
            if F::from_name(name).is_some_and(|f| f.kind() == FieldKind::Uuid))
    }
    match expr {
        FilterExpr::And(l, r) => FilterExpr::And(
            Box::new(accept_quoted_uuids::<F>(l)),
            Box::new(accept_quoted_uuids::<F>(r)),
        ),
        FilterExpr::Or(l, r) => FilterExpr::Or(
            Box::new(accept_quoted_uuids::<F>(l)),
            Box::new(accept_quoted_uuids::<F>(r)),
        ),
        FilterExpr::Not(inner) => FilterExpr::Not(Box::new(accept_quoted_uuids::<F>(inner))),
        FilterExpr::Compare(field, op, value) if is_uuid_field::<F>(field) => {
            FilterExpr::Compare(field.clone(), *op, Box::new(literal(value)))
        }
        FilterExpr::In(field, values) if is_uuid_field::<F>(field) => {
            FilterExpr::In(field.clone(), values.iter().map(literal).collect())
        }
        other => other.clone(),
    }
}

fn cursor_for<R: KeysetRow>(
    row: &R,
    order: &ODataOrderBy,
    filter_hash: Option<&str>,
    direction: &str,
) -> Result<String, toolkit_odata::Error> {
    let mut keys = Vec::with_capacity(order.0.len());
    for key in &order.0 {
        let field = R::Field::from_name(&key.field)
            .ok_or_else(|| toolkit_odata::Error::InvalidOrderByField(key.field.clone()))?;
        let value = row.sort_value(field);
        keys.push(
            encode_cursor_value(&value, field.kind())
                .map_err(|_| toolkit_odata::Error::InvalidCursor)?,
        );
    }
    CursorV1 {
        k: keys,
        o: order.0.first().map_or(SortDir::Desc, |k| k.dir),
        s: order.to_signed_tokens(),
        f: filter_hash.map(str::to_owned),
        d: direction.to_owned(),
    }
    .encode()
    .map_err(|_| toolkit_odata::Error::InvalidCursor)
}

/// Keyset predicate "strictly after the cursor row" in `order` (reversed for `bwd` cursors).
fn after_cursor<F: KeysetField>(
    cursor: &CursorV1,
    order: &ODataOrderBy,
) -> Result<Condition, toolkit_odata::Error> {
    if cursor.k.len() != order.0.len() {
        return Err(toolkit_odata::Error::InvalidCursor);
    }
    let backward = cursor.d == "bwd";
    let mut keys = Vec::with_capacity(order.0.len());
    for (token, key) in cursor.k.iter().zip(&order.0) {
        let field = F::from_name(&key.field)
            .ok_or_else(|| toolkit_odata::Error::InvalidOrderByField(key.field.clone()))?;
        let mut value = parse_cursor_value(field.kind(), token)
            .map_err(|_| toolkit_odata::Error::InvalidCursor)?;
        if let sea_orm::Value::TimeDateTimeWithTimeZone(Some(t)) = &value {
            value = ts::normalize(*t).into();
        }
        let ascending = (key.dir == SortDir::Asc) != backward;
        keys.push((field.sort_expr(), value, ascending));
    }

    let mut any = Condition::any();
    for (i, (expr, value, ascending)) in keys.iter().enumerate() {
        let mut all = Condition::all();
        for (prev, prev_value, _) in &keys[..i] {
            all = all.add(prev.clone().eq(prev_value.clone()));
        }
        all = all.add(if *ascending {
            expr.clone().gt(value.clone())
        } else {
            expr.clone().lt(value.clone())
        });
        any = any.add(all);
    }
    Ok(any)
}

/// One page of `select` (already scoped and restricted to the list's rows) under the query's
/// `$filter`, `$orderby`, `limit` and `cursor`.
///
/// # Errors
/// `OData` for an invalid filter, order or cursor, `Internal` on a database error.
pub async fn paginate<F, M, E>(
    select: SecureSelect<E, Scoped>,
    conn: &impl DBRunner,
    query: &ODataQuery,
    spec: &KeysetSpec<'_, F>,
) -> Result<Page<E::Model>, DomainError>
where
    F: KeysetField,
    M: FieldToColumn<F>,
    E: EntityTrait,
    E::Model: KeysetRow<Field = F>,
{
    let limit = query
        .limit
        .unwrap_or(spec.limits.default)
        .clamp(1, spec.limits.max);

    let order = match &query.cursor {
        Some(cursor) => ODataOrderBy::from_signed_tokens(&cursor.s)
            .map_err(|_| toolkit_odata::Error::InvalidCursor)?,
        None if query.order.is_empty() => ODataOrderBy(vec![OrderKey {
            field: spec.default_order.0.name().to_owned(),
            dir: spec.default_order.1,
        }]),
        None => query.order.clone(),
    }
    .ensure_tiebreaker(spec.tiebreaker.0.name(), spec.tiebreaker.1);
    for key in &order.0 {
        if F::from_name(&key.field).is_none() {
            return Err(toolkit_odata::Error::InvalidOrderByField(key.field.clone()).into());
        }
    }
    // A cursor issued for a filter binds to it; one without a filter hash accepts any filter.
    if let Some(cursor) = &query.cursor
        && let Some(hash) = cursor.f.as_deref()
        && query.filter_hash.as_deref() != Some(hash)
    {
        return Err(toolkit_odata::Error::FilterMismatch.into());
    }

    let mut select = select;
    if let Some(ast) = query.filter.as_deref() {
        let node = convert_expr_to_filter_node::<F>(&accept_quoted_uuids::<F>(ast))
            .map_err(|e| toolkit_odata::Error::InvalidFilter(e.to_string()))?;
        select = select.filter(filter_condition::<F, M>(&node, spec.time_fields)?);
    }
    let backward = query.cursor.as_ref().is_some_and(|c| c.d == "bwd");
    if let Some(cursor) = &query.cursor {
        select = select.filter(after_cursor::<F>(cursor, &order)?);
    }
    for key in &order.0 {
        let field = F::from_name(&key.field)
            .ok_or_else(|| toolkit_odata::Error::InvalidOrderByField(key.field.clone()))?;
        let ascending = (key.dir == SortDir::Asc) != backward;
        select = select.order_by(
            field.sort_expr(),
            if ascending { Order::Asc } else { Order::Desc },
        );
    }
    let mut items = select
        .limit(limit + 1)
        .all(conn)
        .await
        .map_err(map_scope_err)?;

    let has_more = items.len() as u64 > limit;
    if has_more {
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    if backward {
        items.reverse();
    }
    let hash = query.filter_hash.as_deref();
    let next_cursor = match items.last() {
        Some(last) if backward || has_more => Some(cursor_for(last, &order, hash, "fwd")?),
        _ => None,
    };
    let prev_cursor = match items.first() {
        Some(first) if (backward && has_more) || (!backward && query.cursor.is_some()) => {
            Some(cursor_for(first, &order, hash, "bwd")?)
        }
        _ => None,
    };
    Ok(Page {
        items,
        page_info: PageInfo {
            next_cursor,
            prev_cursor,
            limit,
        },
    })
}
