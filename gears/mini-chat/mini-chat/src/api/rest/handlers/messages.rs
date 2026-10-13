//! Message list handler (DESIGN section 3.3, List Messages).

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::{ApiResult, OData};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::MessageDto;
use crate::gear::AppState;

/// `GET {prefix}/v1/chats/{id}/messages`
///
/// # Errors
/// Canonical 400 (path, `OData` query), 403, 404, 500.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(chat_id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MessageDto>>> {
    let page = st.messages.list(&ctx, chat_id, query).await?;
    Ok(Json(page.map_items(MessageDto::from)))
}
