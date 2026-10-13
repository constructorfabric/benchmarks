//! Chat list pagination when `$orderby` contains `title` (DESIGN §3.3 chat list).
//!
//! `chats.title` is nullable. The generic `OData` pager orders and builds cursors on the raw
//! column, so untitled chats break the cursor (a NULL has no cursor encoding and compares as
//! unknown). Here the title key is `COALESCE(title, '')` in the ORDER BY, the cursor value and
//! the cursor predicate; every other key uses its column as the generic pager does.

use sea_orm::sea_query::{Expr, Func, SimpleExpr};
use sea_orm::{Condition, ExprTrait, Order};
use toolkit_db::odata::{
    FieldToColumn, LimitCfg, ODataFieldMapping, encode_cursor_value, filter_node_to_condition,
    parse_cursor_value,
};
use toolkit_db::secure::{DBRunner, Scoped, SecureSelect};
use toolkit_odata::filter::{FilterField, convert_expr_to_filter_node};
use toolkit_odata::{CursorV1, Error as ODataError, ODataOrderBy, Page, PageInfo, SortDir};

use super::ChatODataMapper;
use crate::api::rest::dto::ChatDetailDtoFilterField as F;
use crate::infra::db::entity::chat;

/// True when the effective order of `query` contains `title`.
#[must_use]
pub fn orders_by_title(order: &ODataOrderBy) -> bool {
    order.0.iter().any(|k| k.field == "title")
}

/// Effective order: the cursor's order, or the requested order with the tiebreaker.
///
/// # Errors
/// Invalid cursor order tokens.
pub fn effective_order(
    query: &toolkit_odata::ODataQuery,
    tiebreaker: (&str, SortDir),
) -> Result<ODataOrderBy, ODataError> {
    match &query.cursor {
        Some(cur) => ODataOrderBy::from_signed_tokens(&cur.s).map_err(|_| ODataError::InvalidCursor),
        None => Ok(query.order.clone().ensure_tiebreaker(tiebreaker.0, tiebreaker.1)),
    }
}

/// Sort / compare expression of a field.
fn key_expr(field: F) -> SimpleExpr {
    match field {
        F::Title => Func::coalesce([Expr::col(chat::Column::Title), Expr::val("")]).into(),
        other => Expr::col(<ChatODataMapper as FieldToColumn<F>>::map_field(other)).into(),
    }
}

/// Cursor value of a field (the title as `COALESCE(title, '')`).
fn key_value(m: &chat::Model, field: F) -> sea_orm::Value {
    match field {
        F::Title => sea_orm::Value::String(Some(m.title.clone().unwrap_or_default())),
        other => <ChatODataMapper as ODataFieldMapping<F>>::extract_cursor_value(m, other),
    }
}

fn field_of(name: &str) -> Result<F, ODataError> {
    F::from_name(name).ok_or_else(|| ODataError::InvalidOrderByField(name.to_owned()))
}

fn cursor_predicate(cursor: &CursorV1, order: &ODataOrderBy) -> Result<Condition, ODataError> {
    if cursor.k.len() != order.0.len() {
        return Err(ODataError::InvalidCursor);
    }
    let mut keys = Vec::with_capacity(order.0.len());
    for (i, raw) in cursor.k.iter().enumerate() {
        let ok = &order.0[i];
        let field = field_of(&ok.field)?;
        let kind = <ChatODataMapper as ODataFieldMapping<F>>::cursor_kind(field);
        let value = parse_cursor_value(kind, raw).map_err(|_| ODataError::InvalidCursor)?;
        keys.push((field, value, ok.dir));
    }
    let backward = cursor.d == "bwd";
    let mut any = Condition::any();
    for i in 0..keys.len() {
        let mut all = Condition::all();
        for (field, value, _) in keys.iter().take(i) {
            all = all.add(key_expr(*field).eq(value.clone()));
        }
        let (field, value, dir) = &keys[i];
        let greater = matches!((dir, backward), (SortDir::Asc, false) | (SortDir::Desc, true));
        all = all.add(if greater {
            key_expr(*field).gt(value.clone())
        } else {
            key_expr(*field).lt(value.clone())
        });
        any = any.add(all);
    }
    Ok(any)
}

fn cursor_of(
    m: &chat::Model,
    order: &ODataOrderBy,
    filter_hash: Option<&str>,
    direction: &str,
) -> Result<String, ODataError> {
    let mut k = Vec::with_capacity(order.0.len());
    for ok in &order.0 {
        let field = field_of(&ok.field)?;
        let kind = <ChatODataMapper as ODataFieldMapping<F>>::cursor_kind(field);
        k.push(encode_cursor_value(&key_value(m, field), kind).map_err(|_| ODataError::InvalidCursor)?);
    }
    CursorV1 {
        k,
        o: order.0.first().map_or(SortDir::Desc, |x| x.dir),
        s: order.to_signed_tokens(),
        f: filter_hash.map(str::to_owned),
        d: direction.to_owned(),
    }
    .encode()
    .map_err(|_| ODataError::InvalidCursor)
}

/// Paginates `select` with `order` (which contains `title`), mirroring the generic pager.
///
/// # Errors
/// Invalid filter / order / cursor, database failure.
pub async fn paginate(
    select: SecureSelect<chat::Entity, Scoped>,
    conn: &impl DBRunner,
    query: &toolkit_odata::ODataQuery,
    order: &ODataOrderBy,
    limits: LimitCfg,
) -> Result<Page<chat::Model>, ODataError> {
    let mut limit = std::cmp::max(query.limit.unwrap_or(limits.default), 1);
    if limit > limits.max {
        limit = limits.max;
    }
    for ok in &order.0 {
        field_of(&ok.field)?;
    }
    if let Some(cur) = &query.cursor
        && let Some(cf) = cur.f.as_deref()
        && query.filter_hash.as_deref() != Some(cf)
    {
        return Err(ODataError::FilterMismatch);
    }
    let mut s = select;
    if let Some(ast) = query.filter.as_deref() {
        let node = convert_expr_to_filter_node::<F>(ast)
            .map_err(|e| ODataError::InvalidFilter(e.to_string()))?;
        s = s.filter(
            filter_node_to_condition::<F, ChatODataMapper>(&node).map_err(ODataError::InvalidFilter)?,
        );
    }
    let backward = query.cursor.as_ref().is_some_and(|c| c.d == "bwd");
    if let Some(cur) = &query.cursor {
        s = s.filter(cursor_predicate(cur, order)?);
    }
    for ok in &order.0 {
        let field = field_of(&ok.field)?;
        let asc = matches!((ok.dir, backward), (SortDir::Asc, false) | (SortDir::Desc, true));
        s = s.order_by(key_expr(field), if asc { Order::Asc } else { Order::Desc });
    }
    let mut rows = s
        .limit(limit + 1)
        .all(conn)
        .await
        .map_err(|e| ODataError::Db(e.to_string()))?;
    let has_more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > limit;
    if backward {
        if has_more {
            rows.pop();
        }
        rows.reverse();
    } else if has_more {
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let fh = query.filter_hash.as_deref();
    let next_cursor = if backward || has_more {
        rows.last().map(|m| cursor_of(m, order, fh, "fwd")).transpose()?
    } else {
        None
    };
    let prev_cursor = if (backward && has_more) || (!backward && query.cursor.is_some()) {
        rows.first().map(|m| cursor_of(m, order, fh, "bwd")).transpose()?
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
