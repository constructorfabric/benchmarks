//! REST handlers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::*;
use toolkit::api::odata::OData;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto, ModelDto,
    ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::app::App;
use crate::domain::attachments::{UploadFile, normalize_filename, read_limited};
use crate::domain::error::DomainError;
use crate::domain::stream::{SendInput, StreamStart};
use crate::domain::turns::Mutation;

type AppExt = Extension<Arc<App>>;

fn ping(app: &App) -> Duration {
    Duration::from_secs(u64::from(app.cfg.streaming.sse_ping_interval_seconds))
}

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = app.create_chat(&ctx, req.title, req.model).await?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), view.chat.id);
    Ok((StatusCode::CREATED, [(axum::http::header::LOCATION, location)], Json(ChatDetailDto::from(view))).into_response())
}

pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = app.list_chats(&ctx, &query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(app.get_chat(&ctx, id).await?.into()))
}

pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(app.update_chat_title(&ctx, id, &req.title).await?.into()))
}

pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    app.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = app.list_messages(&ctx, id, &query).await?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

async fn run_setup<F>(fut: F) -> Result<StreamStart, CanonicalError>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    // A disconnect before the stream opens must not interrupt the setup.
    match tokio::spawn(fut).await {
        Ok(r) => r.map_err(CanonicalError::from),
        Err(e) => Err(CanonicalError::from(DomainError::internal(format!("stream setup task failed: {e}")))),
    }
}

pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let input = SendInput {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids.unwrap_or_default(),
        web_search: req.web_search.is_some_and(|w| w.enabled),
    };
    let a = Arc::clone(&app);
    let start = run_setup(async move { a.start_send(ctx, id, input).await }).await?;
    Ok(sse_response(start, ping(&app)))
}

pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(app.get_turn(&ctx, id, request_id).await?.into()))
}

pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let a = Arc::clone(&app);
    let start = run_setup(async move { a.start_mutation(ctx, id, request_id, Mutation::Retry, None).await }).await?;
    Ok(sse_response(start, ping(&app)))
}

pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let a = Arc::clone(&app);
    let start = run_setup(async move { a.start_mutation(ctx, id, request_id, Mutation::Edit, Some(req.content)).await }).await?;
    Ok(sse_response(start, ping(&app)))
}

pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    app.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    Ok(Json(app.set_reaction(&ctx, id, msg_id, &req.reaction).await?.into()))
}

pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    app.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_models(Extension(ctx): Extension<SecurityContext>, Extension(app): AppExt) -> ApiResult<Json<ModelListDto>> {
    let items = app.list_models(&ctx).await?.into_iter().map(ModelDto::from).collect();
    Ok(Json(ModelListDto { items }))
}

pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    Ok(Json(app.get_model(&ctx, &id).await?.into()))
}

pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let list = app.quota_status(&ctx).await?;
    Ok(Json(QuotaStatusResponse::from_status(list, app.cfg.quota.warning_threshold_pct)))
}

pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(app.get_attachment(&ctx, id, attachment_id).await?.into()))
}

pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    app.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn multipart_err(field: &'static str, reason: &'static str, detail: impl Into<String>) -> CanonicalError {
    DomainError::Multipart { field, reason, detail: detail.into() }.into()
}

pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): AppExt,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    let uc = app.upload_context(&ctx, id).await?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let boundary = multer::parse_boundary(&content_type)
        .map_err(|_| multipart_err("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let _slot = Arc::clone(&app.upload_slots)
        .try_acquire_owned()
        .map_err(|_| CanonicalError::from(DomainError::UploadConcurrency))?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => return Err(multipart_err("file", "MISSING_FILE", "multipart field 'file' is required")),
            Err(e) => return Err(multipart_err("multipart", "MULTIPART_ERROR", e.to_string())),
        }
    };
    let part_type = field
        .content_type()
        .map(ToString::to_string)
        .ok_or_else(|| multipart_err("content_type", "MISSING_CONTENT_TYPE", "the file part has no content type"))?;
    let filename = normalize_filename(field.file_name());
    let class = app.validate_upload(&uc, &UploadFile { filename: filename.clone(), content_type: part_type })?;
    let limit = if class.kind == "image" { uc.image_limit_bytes } else { uc.document_limit_bytes };
    let data = read_limited(&mut field, limit).await?;
    let row = app.store_upload(&ctx, &uc, filename, class, data, started).await?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(row))).into_response())
}
