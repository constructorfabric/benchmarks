//! Messages list handler.

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::MiniChatMessageDto;
use crate::domain::services::Services;

/// `GET /v1/chats/{id}/messages`.
///
/// # Errors
/// Canonical problem for query, authorization and storage failures.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<JsonPage<MiniChatMessageDto>> {
    let page = svc.messages.list(&ctx, id, &query).await?;
    let items = page
        .items
        .into_iter()
        .map(MiniChatMessageDto::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(toolkit_odata::Page {
        items,
        page_info: page.page_info,
    }))
}
