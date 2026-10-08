//! REST / SSE handlers.

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse,
    UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::service::AppServices;
use crate::domain::service::attachments::normalize_filename;
use crate::domain::service::stream::{SendRequest, StreamStart};
use crate::domain::service::turns::MutationOp;

pub type Svc = Arc<AppServices>;
type ApiResult<T> = Result<T, CanonicalError>;

fn err(e: DomainError) -> CanonicalError {
    CanonicalError::from(e)
}

/// Runs stream setup in a separate task so a client disconnect does not
/// interrupt it (DESIGN §3.3 "SSE stream close rules", rule 2).
async fn detached<F>(f: F) -> ApiResult<StreamStart>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    tokio::spawn(f)
        .await
        .map_err(|e| CanonicalError::internal(format!("stream setup task failed: {e}")).create())?
        .map_err(err)
}

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Json(body): Json<CreateChatReq>,
) -> ApiResult<Response> {
    let chat = svc.create_chat(&ctx, body.title, body.model).await.map_err(err)?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), chat.id);
    Ok((
        StatusCode::CREATED,
        [(http::header::LOCATION, location)],
        axum::Json(ChatDetailDto::from(chat)),
    )
        .into_response())
}

pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    OData(q): OData,
) -> ApiResult<axum::Json<Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, &q).await.map_err(err)?;
    Ok(axum::Json(Page {
        items: page.items.into_iter().map(ChatDetailDto::from).collect(),
        page_info: page.page_info,
    }))
}

pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(svc.get_chat(&ctx, id).await.map_err(err)?.into()))
}

pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateChatReq>,
) -> ApiResult<axum::Json<ChatDetailDto>> {
    Ok(axum::Json(
        svc.update_chat_title(&ctx, id, &body.title).await.map_err(err)?.into(),
    ))
}

pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    svc.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
    OData(q): OData,
) -> ApiResult<axum::Json<Page<MiniChatMessageDto>>> {
    let page = svc.list_messages(&ctx, id, &q).await.map_err(err)?;
    Ok(axum::Json(Page {
        items: page.items.into_iter().map(MiniChatMessageDto::from).collect(),
        page_info: page.page_info,
    }))
}

pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendRequest {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    let start = detached(async move { svc.send_message(&ctx, id, req).await }).await?;
    Ok(sse_response(start))
}

pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<TurnStatusResponse>> {
    Ok(axum::Json(svc.get_turn(&ctx, id, request_id).await.map_err(err)?.into()))
}

pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let start =
        detached(async move { svc.mutate_turn(&ctx, id, request_id, MutationOp::Retry, None).await }).await?;
    Ok(sse_response(start))
}

pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let start = detached(async move {
        svc.mutate_turn(&ctx, id, request_id, MutationOp::Edit, Some(body.content))
            .await
    })
    .await?;
    Ok(sse_response(start))
}

pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_turn(&ctx, id, request_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetReactionReq>,
) -> ApiResult<axum::Json<MiniChatReactionDto>> {
    Ok(axum::Json(
        svc.set_reaction(&ctx, id, msg_id, &body.reaction).await.map_err(err)?.into(),
    ))
}

pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_reaction(&ctx, id, msg_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
) -> ApiResult<axum::Json<ModelListDto>> {
    let items = svc.list_models(&ctx).await.map_err(err)?;
    Ok(axum::Json(ModelListDto {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<axum::Json<ModelDto>> {
    Ok(axum::Json(svc.get_model(&ctx, &id).await.map_err(err)?.into()))
}

pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
) -> ApiResult<axum::Json<QuotaStatusResponse>> {
    let (entries, pct) = svc.get_quota_status(&ctx).await.map_err(err)?;
    Ok(axum::Json(QuotaStatusResponse::from_status(entries, pct)))
}

pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<AttachmentDetailDto>> {
    Ok(axum::Json(
        svc.get_attachment(&ctx, id, attachment_id).await.map_err(err)?.into(),
    ))
}

pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_attachment(&ctx, id, attachment_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

fn multipart_err(field: &'static str, reason: &'static str, detail: impl Into<String>) -> CanonicalError {
    err(DomainError::Multipart(field, reason, detail.into()))
}

/// `POST /v1/chats/{id}/attachments` (streaming multipart, byte-counted).
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    // Chat and model are resolved before the body is read.
    let up = svc.prepare_upload(&ctx, id).await.map_err(err)?;
    let ct = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(ct)
        .map_err(|_| multipart_err("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    let field = loop {
        match mp.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => return Err(multipart_err("file", "MISSING_FILE", "multipart field 'file' is required")),
            Err(e) => return Err(multipart_err("multipart", "MULTIPART_ERROR", e.to_string())),
        }
    };
    let raw_ct = field
        .content_type()
        .map(ToString::to_string)
        .ok_or_else(|| multipart_err("content_type", "MISSING_CONTENT_TYPE", "file part has no content type"))?;
    let filename = normalize_filename(field.file_name());
    let (mime, kind, for_fs, for_ci) = svc.validate_upload_type(&up, &raw_ct, &filename).map_err(err)?;
    let limit = up.limit_bytes(kind, &svc.cfg.rag);
    let mut buf = BytesMut::new();
    let mut field = field;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > limit {
                    return Err(err(DomainError::FileTooLarge));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(multipart_err("multipart", "MULTIPART_ERROR", e.to_string())),
        }
    }
    let data: Bytes = buf.freeze();
    let view = svc
        .store_upload(&ctx, up, filename, mime, kind, for_fs, for_ci, data, started)
        .await
        .map_err(err)?;
    Ok((StatusCode::CREATED, axum::Json(AttachmentDetailDto::from(view))).into_response())
}

/// Drains an unused stream (kept for symmetry with streaming bodies).
#[allow(dead_code)]
async fn drain(mut s: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin) {
    while s.next().await.is_some() {}
}
