//! REST handlers.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto, ModelDto,
    ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse,
    UpdateChatReq,
};
use super::sse::stream_response;
use crate::domain::error::DomainError;
use crate::domain::service::Svc;
use crate::domain::stream_service::{SendRequest, StreamStart};
use crate::domain::turn_service::Mutation;

type ApiResult<T> = Result<T, CanonicalError>;

fn join_err(e: &tokio::task::JoinError) -> CanonicalError {
    CanonicalError::internal(format!("setup task failed: {e}")).create()
}

/// `POST /chats`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Json(body): Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = svc.create_chat(&ctx, body.title, body.model).await?;
    let id = view.chat.id.to_string();
    let location = format!("{}/{id}", uri.path().trim_end_matches('/'));
    Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)], axum::Json(ChatDetailDto::from(view))).into_response())
}

/// `GET /chats`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, &query).await?;
    Ok(axum::Json(page.map_items(ChatDetailDto::from)))
}

/// `GET /chats/{id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(svc.get_chat(&ctx, id).await?.into()))
}

/// `PATCH /chats/{id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateChatReq>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(svc.rename_chat(&ctx, id, &body.title).await?.into()))
}

/// `DELETE /chats/{id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    svc.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /chats/{id}/messages`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<MiniChatMessageDto>>> {
    let page = svc.list_messages(&ctx, id, &query).await?;
    let items = page
        .items
        .into_iter()
        .map(MiniChatMessageDto::try_from_view)
        .collect::<Result<Vec<_>, DomainError>>()?;
    Ok(axum::Json(Page { items, page_info: page.page_info }))
}

async fn run_setup<F>(fut: F) -> ApiResult<StreamStart>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    Ok(tokio::spawn(fut).await.map_err(|e| join_err(&e))??)
}

/// `POST /chats/{id}/messages:stream`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendRequest {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    let s = Arc::clone(&svc);
    let start = run_setup(async move { s.prepare_send(&ctx, id, req).await }).await?;
    Ok(stream_response(svc, start))
}

/// `GET /chats/{id}/turns/{request_id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<TurnStatusResponse>> {
    Ok(axum::Json(svc.turn_status(&ctx, id, request_id).await?.into()))
}

/// `POST /chats/{id}/turns/{request_id}/retry`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let s = Arc::clone(&svc);
    let start = run_setup(async move { s.mutate_turn(&ctx, id, request_id, Mutation::Retry).await }).await?;
    Ok(stream_response(svc, start))
}

/// `PATCH /chats/{id}/turns/{request_id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let s = Arc::clone(&svc);
    let start = run_setup(async move { s.mutate_turn(&ctx, id, request_id, Mutation::Edit(body.content)).await }).await?;
    Ok(stream_response(svc, start))
}

/// `DELETE /chats/{id}/turns/{request_id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /chats/{id}/messages/{msg_id}/reaction`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetReactionReq>,
) -> ApiResult<axum::Json<MiniChatReactionDto>> {
    Ok(axum::Json(svc.set_reaction(&ctx, id, msg_id, &body.reaction).await?.into()))
}

/// `DELETE /chats/{id}/messages/{msg_id}/reaction`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /models`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
) -> ApiResult<axum::Json<ModelListDto>> {
    let models = svc.list_models(&ctx).await?;
    Ok(axum::Json(ModelListDto { items: models.iter().map(ModelDto::from).collect() }))
}

/// `GET /models/{id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<axum::Json<ModelDto>> {
    let m = svc.get_model(&ctx, &id).await?;
    Ok(axum::Json(ModelDto::from(&m)))
}

/// `GET /quota/status`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
) -> ApiResult<axum::Json<QuotaStatusResponse>> {
    let entries = svc.quota_status(&ctx).await?;
    Ok(axum::Json(QuotaStatusResponse::from_entries(&entries, svc.cfg.quota.warning_threshold_pct)))
}
