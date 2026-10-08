//! REST handlers: extract, call the service, map to DTOs.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use toolkit::api::canonical_prelude::{ApiResult, OData};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto, ModelDto,
    ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::error::{DomainError, Res};
use crate::service::AppState;
use crate::service::attachments::{classify, file_too_large};
use crate::service::stream::{SendRequest, StreamStart};

type State = Extension<Arc<AppState>>;
type Ctx = Extension<SecurityContext>;

fn ping_every(state: &AppState) -> Duration {
    Duration::from_secs(u64::from(state.cfg.streaming.sse_ping_interval_seconds))
}

/// Run stream setup in its own task so a client disconnect before the
/// stream opens does not interrupt it.
async fn detached<F>(fut: F) -> Result<StreamStart, CanonicalError>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(r) => r.map_err(CanonicalError::from),
        Err(e) => Err(DomainError::internal(format!("stream setup task failed: {e}")).into()),
    }
}

// ---------------------------------------------------------------------------
// Chats
// ---------------------------------------------------------------------------

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn create_chat(
    Extension(state): State,
    Extension(ctx): Ctx,
    uri: Uri,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = state.create_chat(&ctx, req.title, req.model).await?;
    let id = view.chat.id;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), id);
    let mut resp = (StatusCode::CREATED, Json(ChatDetailDto::from(view))).into_response();
    if let Ok(v) = HeaderValue::from_str(&location) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    Ok(resp)
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn list_chats(
    Extension(state): State,
    Extension(ctx): Ctx,
    OData(query): OData,
) -> ApiResult<Json<toolkit_odata::Page<ChatDetailDto>>> {
    let page = state.list_chats(&ctx, &query).await?;
    Ok(Json(toolkit_odata::Page {
        items: page.items.into_iter().map(ChatDetailDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn get_chat(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(state.get_chat(&ctx, id).await?.into()))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn update_chat(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(
        state.update_chat_title(&ctx, id, &req.title).await?.into(),
    ))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn delete_chat(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Messages / turns
// ---------------------------------------------------------------------------

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn list_messages(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<toolkit_odata::Page<MessageDto>>> {
    let page = state.list_messages(&ctx, id, &query).await?;
    Ok(Json(toolkit_odata::Page {
        items: page.items.into_iter().map(MessageDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn stream_message(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Json(req): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let st = Arc::clone(&state);
    let send = SendRequest {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids,
        web_search: req.web_search.is_some_and(|w| w.enabled),
    };
    let start = detached(async move { st.start_send(ctx, id, send).await }).await?;
    Ok(sse_response(start, ping_every(&state)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn get_turn(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let v = state.get_turn_status(&ctx, id, request_id).await?;
    Ok(Json(TurnStatusResponse::from(&v.turn)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn retry_turn(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let st = Arc::clone(&state);
    let start =
        detached(async move { st.mutate_and_stream(ctx, id, request_id, None).await }).await?;
    Ok(sse_response(start, ping_every(&state)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn edit_turn(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let st = Arc::clone(&state);
    let start = detached(async move {
        st.mutate_and_stream(ctx, id, request_id, Some(req.content))
            .await
    })
    .await?;
    Ok(sse_response(start, ping_every(&state)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn delete_turn(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn put_reaction(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<Json<ReactionDto>> {
    Ok(Json(
        state
            .set_reaction(&ctx, id, msg_id, &req.reaction)
            .await?
            .into(),
    ))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn delete_reaction(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

fn bad(field: &str, reason: &str, desc: &str) -> CanonicalError {
    DomainError::invalid(Res::Attachment, field, reason, desc).into()
}

fn multipart_error(e: &multer::Error) -> CanonicalError {
    tracing::debug!(error = %e, "multipart read failed");
    bad(
        "multipart",
        "MULTIPART_ERROR",
        "the multipart body could not be read",
    )
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn upload_attachment(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    // Chat, model and limits are resolved before the body is read.
    let target = state.prepare_upload(&ctx, id).await?;
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let boundary = multer::parse_boundary(ct).map_err(|_| {
        bad(
            "content_type",
            "BOUNDARY_REQUIRED",
            "multipart boundary is required",
        )
    })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => return Err(bad("file", "MISSING_FILE", "the 'file' field is missing")),
            Err(e) => return Err(multipart_error(&e)),
        }
    };
    let part_ct = field
        .content_type()
        .map(ToString::to_string)
        .ok_or_else(|| {
            bad(
                "content_type",
                "MISSING_CONTENT_TYPE",
                "the file part has no content type",
            )
        })?;
    let filename = field.file_name().unwrap_or("upload").to_owned();
    let mut class = classify(&part_ct, &filename, state.cfg.rag.allow_csv_upload)?;
    state.filter_purposes(&target, &mut class)?;
    let limit = target.limit_bytes(&state.cfg.rag, class.is_image);
    let mut buf = BytesMut::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > limit {
                    return Err(file_too_large().into());
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(multipart_error(&e)),
        }
    }
    let row = state
        .upload_attachment(&ctx, target, &filename, class, buf.freeze())
        .await?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(&row))).into_response())
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn get_attachment(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let a = state.get_attachment(&ctx, id, attachment_id).await?;
    Ok(Json(AttachmentDetailDto::from(&a)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn delete_attachment(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Models / quota
// ---------------------------------------------------------------------------

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn list_models(
    Extension(state): State,
    Extension(ctx): Ctx,
) -> ApiResult<Json<ModelListDto>> {
    let models = state.list_models(&ctx).await?;
    Ok(Json(ModelListDto {
        items: models.iter().map(ModelDto::from).collect(),
    }))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn get_model(
    Extension(state): State,
    Extension(ctx): Ctx,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let m = state.get_model(&ctx, &id).await?;
    Ok(Json(ModelDto::from(&m)))
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn get_quota_status(
    Extension(state): State,
    Extension(ctx): Ctx,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let tiers = state.get_quota_status(&ctx).await?;
    Ok(Json(QuotaStatusResponse::new(
        tiers,
        state.cfg.quota.warning_threshold_pct,
    )))
}
