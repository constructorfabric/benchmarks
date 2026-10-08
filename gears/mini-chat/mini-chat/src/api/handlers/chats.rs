//! Chat CRUD handlers (DESIGN §3.3 "Create/List/Get/Update/Delete Chat").

use std::sync::Arc;

use axum::Extension;
use axum::http::Uri;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::{ApiResult, IntoResponse, StatusCode};
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType};
use toolkit::api::rest::extract::{Json, Path};
use toolkit::api::{OpenApiRegistry, ThrottlingSpec};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{License, V1};
use crate::domain::chats::{self, ChatField, ChatView};
use crate::domain::services::AppServices;

const TAG: &str = "Mini Chat Chats";

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<ChatView> for ChatDetailDto {
    fn from(v: ChatView) -> Self {
        Self {
            id: v.chat.id,
            model: v.chat.model,
            title: v.chat.title,
            is_temporary: v.chat.is_temporary,
            message_count: v.message_count,
            created_at: v.chat.created_at,
            updated_at: v.chat.updated_at,
        }
    }
}

/// `POST /chats`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let view = chats::create_chat(&app, &ctx, req.title, req.model).await?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), view.chat.id);
    Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)], axum::Json(ChatDetailDto::from(view))))
}

/// `GET /chats`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<ChatDetailDto>>> {
    let page = chats::list_chats(&app, &ctx, &query).await?;
    Ok(axum::Json(page.map_items(ChatDetailDto::from)))
}

/// `GET /chats/{id}`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(chats::get_chat(&app, &ctx, id).await?.into()))
}

/// `PATCH /chats/{id}`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(chats::update_chat_title(&app, &ctx, id, &req.title).await?.into()))
}

/// `DELETE /chats/{id}`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    chats::delete_chat(&app, &ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer)
}

/// Registers this area's routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let chats_path = format!("{V1}/chats");
    let chat_path = format!("{V1}/chats/{{id}}");

    let router = OperationBuilder::post(chats_path.clone())
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG)
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".into()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".into()),
            require_security_context: false,
            dry_run: true,
        })
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(ResponseHeaderSpec::new("Location", "Path of the created chat", ResponseHeaderType::String))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(chats_path)
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .query_param_typed("limit", false, "Maximum number of chats to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .handler(list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Paginated list of chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(chat_path.clone())
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .handler(get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(chat_path.clone())
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::delete(chat_path)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .handler(delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
