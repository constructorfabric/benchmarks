//! REST handlers.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq,
    StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::odata::ListQuery;
use super::sse::sse_response;
use crate::domain::error::{DomainError, MultipartFailure};
use crate::domain::mime;
use crate::domain::service::Service;
use crate::domain::service::stream::{SendRequest, StreamStart};

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Service>,
}

fn err(e: DomainError) -> CanonicalError {
    e.into()
}

fn ping_interval(svc: &Service) -> Duration {
    Duration::from_secs(u64::from(svc.cfg.streaming.sse_ping_interval_seconds))
}

/// Run a stream setup in a spawned task so a client disconnect does not
/// interrupt it; the response is built from its result.
async fn spawned<F>(fut: F) -> Result<StreamStart, CanonicalError>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(r) => r.map_err(err),
        Err(e) => Err(err(DomainError::internal(format!(
            "stream setup task failed: {e}"
        )))),
    }
}

// ---------------------------------------------------------------- chats --

pub async fn create_chat(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = state
        .service
        .create_chat(&ctx, req.title, req.model)
        .await
        .map_err(err)?;
    let location = format!(
        "{}/v1/chats/{}",
        state.service.cfg.url_prefix.trim_end_matches('/'),
        view.chat.id
    );
    let mut resp = (StatusCode::CREATED, Json(ChatDetailDto::from(view))).into_response();
    if let Ok(v) = HeaderValue::from_str(&location) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    Ok(resp)
}

pub async fn list_chats(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    ListQuery(query): ListQuery,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = state.service.list_chats(&ctx, &query).await.map_err(err)?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

pub async fn get_chat(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    let view = state.service.get_chat(&ctx, id).await.map_err(err)?;
    Ok(Json(view.into()))
}

pub async fn update_chat(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    let view = state
        .service
        .update_chat_title(&ctx, id, &req.title)
        .await
        .map_err(err)?;
    Ok(Json(view.into()))
}

pub async fn delete_chat(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.service.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------- messages --

pub async fn list_messages(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
    ListQuery(query): ListQuery,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = state
        .service
        .list_messages(&ctx, id, &query)
        .await
        .map_err(err)?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

pub async fn stream_message(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let svc = Arc::clone(&state.service);
    let ping = ping_interval(&svc);
    let request = SendRequest {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids,
        web_search: req.web_search.is_some_and(|w| w.enabled),
    };
    let start = spawned(async move { svc.send_message(&ctx, id, request).await }).await?;
    Ok(sse_response(start, ping))
}

pub async fn put_reaction(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let r = state
        .service
        .set_reaction(&ctx, id, msg_id, &req.reaction)
        .await
        .map_err(err)?;
    Ok(Json(r.into()))
}

pub async fn delete_reaction(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state
        .service
        .delete_reaction(&ctx, id, msg_id)
        .await
        .map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- turns --

pub async fn get_turn(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let t = state
        .service
        .get_turn(&ctx, id, request_id)
        .await
        .map_err(err)?;
    Ok(Json(t.into()))
}

pub async fn retry_turn(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let svc = Arc::clone(&state.service);
    let ping = ping_interval(&svc);
    let start =
        spawned(async move { svc.regenerate_turn(&ctx, id, request_id, None).await }).await?;
    Ok(sse_response(start, ping))
}

pub async fn edit_turn(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let svc = Arc::clone(&state.service);
    let ping = ping_interval(&svc);
    let start = spawned(async move {
        svc.regenerate_turn(&ctx, id, request_id, Some(req.content))
            .await
    })
    .await?;
    Ok(sse_response(start, ping))
}

pub async fn delete_turn(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state
        .service
        .delete_turn(&ctx, id, request_id)
        .await
        .map_err(|e| match e {
            DomainError::OutboxPayloadTooLarge { detail } => err(DomainError::internal(detail)),
            other => err(other),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------- attachments --

fn multipart_err(failure: MultipartFailure, detail: impl Into<String>) -> CanonicalError {
    err(DomainError::Multipart {
        failure,
        detail: detail.into(),
    })
}

pub async fn upload_attachment(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let svc = &state.service;
    let target = svc.prepare_upload(&ctx, id).await.map_err(err)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type).map_err(|_| {
        multipart_err(
            MultipartFailure::BoundaryRequired,
            "The multipart Content-Type has no boundary",
        )
    })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(multipart_err(
                    MultipartFailure::MissingFile,
                    "The multipart body has no 'file' field",
                ));
            }
            Err(e) => {
                return Err(multipart_err(
                    MultipartFailure::Unreadable,
                    format!("Unreadable multipart body: {e}"),
                ));
            }
        }
    };
    let filename = mime::normalize_filename(field.file_name());
    let part_type = field
        .content_type()
        .map(ToString::to_string)
        .ok_or_else(|| {
            multipart_err(
                MultipartFailure::MissingContentType,
                "The 'file' part has no Content-Type",
            )
        })?;
    let mut mime_type = mime::normalize(&part_type);
    if mime_type == "application/octet-stream"
        && let Some(inferred) = mime::from_extension(&filename)
    {
        mime_type = inferred.to_owned();
    }
    let plan = svc.plan_upload(&target, &mime_type).map_err(err)?;
    let mut buf = BytesMut::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > plan.max_bytes {
                    return Err(err(DomainError::FileTooLarge {
                        limit_bytes: plan.max_bytes,
                    }));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(multipart_err(
                    MultipartFailure::Unreadable,
                    format!("Unreadable multipart body: {e}"),
                ));
            }
        }
    }
    let data: Bytes = buf.freeze();
    let row = svc
        .store_upload(&ctx, target, plan, filename, data)
        .await
        .map_err(err)?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(row))).into_response())
}

pub async fn get_attachment(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let a = state
        .service
        .get_attachment(&ctx, id, attachment_id)
        .await
        .map_err(err)?;
    Ok(Json(a.into()))
}

pub async fn delete_attachment(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state
        .service
        .delete_attachment(&ctx, id, attachment_id)
        .await
        .map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- models --

pub async fn list_models(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Json<ModelListDto>> {
    let items = state.service.list_models(&ctx).await.map_err(err)?;
    Ok(Json(ModelListDto {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

pub async fn get_model(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let m = state.service.get_model(&ctx, &id).await.map_err(err)?;
    Ok(Json(m.into()))
}

// ---------------------------------------------------------------- quota --

pub async fn get_quota_status(
    Extension(state): Extension<AppState>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let rows = state.service.quota_status(&ctx).await.map_err(err)?;
    Ok(Json(QuotaStatusResponse::new(
        rows,
        state.service.cfg.quota.warning_threshold_pct,
    )))
}
