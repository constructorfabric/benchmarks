//! Axum handlers. Pre-stream failures are canonical `Problem`s; streaming
//! endpoints answer with SSE.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{ApiResult, OData, created_json, no_content};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto, ModelDto,
    ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::service::Services;
use crate::domain::stream::{SendRequest, TurnStart};
use crate::domain::turns::Mutation;

type Svc = Extension<Arc<Services>>;
type Ctx = Extension<SecurityContext>;

fn err(e: DomainError) -> CanonicalError {
    CanonicalError::from(e)
}

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc
        .create_chat(&ctx, req.title, req.model)
        .await
        .map_err(err)?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

pub async fn list_chats(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, query).await.map_err(err)?;
    Ok(axum::Json(page.map_items(ChatDetailDto::from)))
}

pub async fn get_chat(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    let chat = svc.get_chat(&ctx, id).await.map_err(err)?;
    Ok(axum::Json(chat.into()))
}

pub async fn update_chat(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    let chat = svc
        .update_chat_title(&ctx, id, &req.title)
        .await
        .map_err(err)?;
    Ok(axum::Json(chat.into()))
}

pub async fn delete_chat(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(no_content())
}

pub async fn list_messages(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<MessageDto>>> {
    let page = svc.list_messages(&ctx, id, query).await.map_err(err)?;
    Ok(axum::Json(page.map_items(MessageDto::from)))
}

/// Run a stream setup in a separate task so a client disconnect does not
/// interrupt it; answer with SSE or a `Problem`.
async fn stream_setup<F>(fut: F) -> Response
where
    F: std::future::Future<Output = Result<TurnStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(Ok(start)) => sse_response(start),
        Ok(Err(e)) => err(e).into_response(),
        Err(e) => err(DomainError::internal(format!(
            "stream setup task failed: {e}"
        )))
        .into_response(),
    }
}

pub async fn stream_message(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
    Json(req): Json<StreamMessageRequest>,
) -> Response {
    let send = SendRequest {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids,
        web_search: req.web_search.is_some_and(|w| w.enabled),
    };
    stream_setup(async move { svc.start_send(&ctx, id, send).await }).await
}

pub async fn retry_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> Response {
    stream_setup(async move {
        svc.start_mutation(&ctx, id, request_id, Mutation::Retry)
            .await
    })
    .await
}

pub async fn edit_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditTurnRequest>,
) -> Response {
    stream_setup(async move {
        svc.start_mutation(&ctx, id, request_id, Mutation::Edit(req.content))
            .await
    })
    .await
}

pub async fn delete_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_turn(&ctx, id, request_id).await.map_err(err)?;
    Ok(no_content())
}

pub async fn get_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<TurnStatusResponse>> {
    let t = svc.turn_status(&ctx, id, request_id).await.map_err(err)?;
    Ok(axum::Json(t.into()))
}

pub async fn upload_attachment(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match svc
        .upload_attachment(&ctx, id, ct, body.into_data_stream())
        .await
    {
        Ok(a) => (
            StatusCode::CREATED,
            axum::Json(AttachmentDetailDto::from(a)),
        )
            .into_response(),
        Err(e) => err(e).into_response(),
    }
}

pub async fn get_attachment(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<AttachmentDetailDto>> {
    let a = svc
        .get_attachment(&ctx, id, attachment_id)
        .await
        .map_err(err)?;
    Ok(axum::Json(a.into()))
}

pub async fn delete_attachment(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_attachment(&ctx, id, attachment_id)
        .await
        .map_err(err)?;
    Ok(no_content())
}

pub async fn put_reaction(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<axum::Json<ReactionDto>> {
    let r = svc
        .set_reaction(&ctx, id, msg_id, &req.reaction)
        .await
        .map_err(err)?;
    Ok(axum::Json(r.into()))
}

pub async fn delete_reaction(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.delete_reaction(&ctx, id, msg_id).await.map_err(err)?;
    Ok(no_content())
}

pub async fn list_models(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
) -> ApiResult<axum::Json<ModelListDto>> {
    let models = svc.list_models(&ctx).await.map_err(err)?;
    Ok(axum::Json(ModelListDto {
        items: models.into_iter().map(ModelDto::from).collect(),
    }))
}

pub async fn get_model(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    Path(id): Path<String>,
) -> ApiResult<axum::Json<ModelDto>> {
    let m = svc.get_model(&ctx, &id).await.map_err(err)?;
    Ok(axum::Json(m.into()))
}

pub async fn quota_status(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
) -> ApiResult<axum::Json<QuotaStatusResponse>> {
    let s = svc.quota_status(&ctx).await.map_err(err)?;
    Ok(axum::Json(s.into()))
}
