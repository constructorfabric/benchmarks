//! Message list handler.

use super::prelude::*;

/// `GET /v1/chats/{id}/messages` — list chat messages.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = svc.list_messages(&ctx, id, query).await.map_err(err)?;
    let PageInfo { next_cursor, prev_cursor, limit } = page.page_info;
    let items = page
        .items
        .into_iter()
        .map(MiniChatMessageDto::try_from_view)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    Ok(Json(Page { items, page_info: PageInfo { next_cursor, prev_cursor, limit } }))
}
