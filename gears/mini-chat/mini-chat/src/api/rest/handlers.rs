//! REST handlers: extract, call the domain service, map the result.

use std::sync::Arc;

use axum::Extension;
use axum::http::{Uri, header};
use axum::response::{IntoResponse, Response};
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::{Json, Path};
use toolkit::api::canonical_prelude::{created_json, no_content, ok_json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::{Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse,
    UpdateChatReq,
};
use super::error::{to_canonical, to_canonical_strict};
use super::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::services::MiniChatService;
use crate::domain::services::attachments::UploadInput;
use crate::domain::services::stream::{SendRequest, StreamStart};
use crate::domain::services::turns::MutationKind;

type Svc = Extension<Arc<MiniChatService>>;
type Ctx = Extension<SecurityContext>;
type ApiResult<T> = Result<T, CanonicalError>;

fn err(e: DomainError) -> CanonicalError {
    to_canonical(e)
}

/// Runs stream setup in its own task so a client disconnect during setup
/// does not abort it half-way.
async fn spawn_setup<F>(f: F) -> Result<StreamStart, CanonicalError>
where
    F: Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(f).await {
        Ok(r) => r.map_err(to_canonical),
        Err(e) => Err(to_canonical(DomainError::internal(format!("stream setup task failed: {e}")))),
    }
}

// --- chats -------------------------------------------------------------------

/// Create a chat.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn create_chat(
    uri: Uri,
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Json(body): Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = svc.create_chat(&ctx, body.title, body.model).await.map_err(err)?;
    let id = view.chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(view), &uri, &id).into_response())
}

/// List chats (`OData` paging).
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn list_chats(Extension(svc): Svc, Extension(ctx): Ctx, OData(query): OData) -> ApiResult<Response> {
    let page = svc.list_chats(&ctx, &query).await.map_err(err)?;
    Ok(ok_json(page.map_items(ChatDetailDto::from)).into_response())
}

/// Get a chat by id.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn get_chat(Extension(svc): Svc, Extension(ctx): Ctx, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let view = svc.get_chat(&ctx, id).await.map_err(err)?;
    Ok(ok_json(ChatDetailDto::from(view)).into_response())
}

/// Update a chat title.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn update_chat(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateChatReq>,
) -> ApiResult<Response> {
    let view = svc.update_chat(&ctx, id, &body.title).await.map_err(err)?;
    Ok(ok_json(ChatDetailDto::from(view)).into_response())
}

/// Delete a chat.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn delete_chat(Extension(svc): Svc, Extension(ctx): Ctx, Path(id): Path<Uuid>) -> ApiResult<Response> {
    svc.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(no_content().into_response())
}

// --- messages ----------------------------------------------------------------

/// List messages of a chat.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn list_messages(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Response> {
    let page = svc.list_messages(&ctx, id, &query).await.map_err(err)?;
    let items = page
        .items
        .into_iter()
        .map(MiniChatMessageDto::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let out: Page<MiniChatMessageDto> = Page {
        items,
        page_info: PageInfo { ..page.page_info },
    };
    Ok(ok_json(out).into_response())
}

/// Send a message and stream the reply over SSE.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn stream_message(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendRequest {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids,
        web_search_enabled: body.web_search.is_some_and(|w| w.enabled),
    };
    let start = spawn_setup(async move { svc.send_message(&ctx, id, req).await }).await?;
    Ok(sse_response(start))
}

// --- turns -------------------------------------------------------------------

/// Get turn status.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn get_turn(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let v = svc.turn_status(&ctx, id, request_id).await.map_err(err)?;
    Ok(ok_json(TurnStatusResponse::from(v)).into_response())
}

/// Retry a turn and stream the new reply.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn retry_turn(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let start = match tokio::spawn(async move {
        svc.mutate_turn(&ctx, MutationKind::Retry, id, request_id, None).await
    })
    .await
    {
        Ok(r) => r.map_err(to_canonical_strict)?,
        Err(e) => return Err(to_canonical(DomainError::internal(format!("retry task failed: {e}")))),
    };
    Ok(sse_response(StreamStart::Live(start)))
}

/// Edit a turn and stream the new reply.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn edit_turn(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let start = match tokio::spawn(async move {
        svc.mutate_turn(&ctx, MutationKind::Edit, id, request_id, Some(body.content))
            .await
    })
    .await
    {
        Ok(r) => r.map_err(to_canonical_strict)?,
        Err(e) => return Err(to_canonical(DomainError::internal(format!("edit task failed: {e}")))),
    };
    Ok(sse_response(StreamStart::Live(start)))
}

/// Delete a turn.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn delete_turn(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_turn(&ctx, id, request_id).await.map_err(to_canonical_strict)?;
    Ok(no_content().into_response())
}

// --- attachments -------------------------------------------------------------

/// Upload an attachment.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn upload_attachment(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let body = req.into_body().into_data_stream();
    let row = svc
        .upload_attachment(&ctx, id, UploadInput { content_type, body })
        .await
        .map_err(err)?;
    Ok((axum::http::StatusCode::CREATED, axum::Json(AttachmentDetailDto::from(row))).into_response())
}

/// Get an attachment.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn get_attachment(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let row = svc.get_attachment(&ctx, id, attachment_id).await.map_err(err)?;
    Ok(ok_json(AttachmentDetailDto::from(row)).into_response())
}

/// Delete an attachment.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn delete_attachment(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_attachment(&ctx, id, attachment_id)
        .await
        .map_err(to_canonical_strict)?;
    Ok(no_content().into_response())
}

// --- reactions ---------------------------------------------------------------

/// Set a reaction on a message.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn put_reaction(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetReactionReq>,
) -> ApiResult<Response> {
    let r = svc.set_reaction(&ctx, id, msg_id, &body.reaction).await.map_err(err)?;
    Ok(ok_json(MiniChatReactionDto::from(r)).into_response())
}

/// Remove a reaction from a message.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn delete_reaction(
    Extension(svc): Svc,
    Extension(ctx): Ctx,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_reaction(&ctx, id, msg_id).await.map_err(err)?;
    Ok(no_content().into_response())
}

// --- models & quota ----------------------------------------------------------

/// List available models.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn list_models(Extension(svc): Svc, Extension(ctx): Ctx) -> ApiResult<Response> {
    let models = svc.list_models(&ctx).await.map_err(err)?;
    Ok(ok_json(ModelListDto {
        items: models.into_iter().map(ModelDto::from).collect(),
    })
    .into_response())
}

/// Get a model by id.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn get_model(Extension(svc): Svc, Extension(ctx): Ctx, Path(id): Path<String>) -> ApiResult<Response> {
    let m = svc.get_model(&ctx, &id).await.map_err(err)?;
    Ok(ok_json(ModelDto::from(m)).into_response())
}

/// Get the caller quota status.
///
/// # Errors
/// Canonical error responses (ADR-0004).
pub async fn get_quota_status(Extension(svc): Svc, Extension(ctx): Ctx) -> ApiResult<Response> {
    let v = svc.quota_status(&ctx).await.map_err(err)?;
    Ok(ok_json(QuotaStatusResponse::from(v)).into_response())
}
