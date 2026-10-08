//! Message list handler (OWNER: REST CRUD work package).

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::Services;

/// `GET /v1/chats/{id}/messages`.
pub async fn list_messages(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Response> {
    let page = svc.messages.list(&ctx, id, query).await?;
    Ok(axum::Json(page).into_response())
}
