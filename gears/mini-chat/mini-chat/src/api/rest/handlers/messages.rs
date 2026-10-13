//! `GET /v1/chats/{id}/messages` — paginated message history.

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::{ApiResult, OData};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::MiniChatMessageDto;
use crate::domain::services::AppServices;

/// Messages of the chat (`OData` `$filter` / `$orderby` over `created_at`,
/// `id`, `role`; default `created_at asc`).
///
/// # Errors
/// 404 (`chat`), 400 (`OData` query, non-UUID id), 403 / 503 from
/// authorization, 500.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = svc.messages.list(&ctx, id, &query).await?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}
