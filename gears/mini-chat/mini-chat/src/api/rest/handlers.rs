//! Axum handlers for the mini-chat REST API.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;

use axum::http::{HeaderMap, Uri};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Sse};
use futures::StreamExt;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    CreateChatReq, EditTurnRequest, MiniChatSseEvent, PingData, SetReactionReq,
    StreamMessageRequest, UpdateChatReq,
};
use crate::domain::attachment_service::UploadInput;
use crate::domain::services::AppServices;
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;

/// `POST /v1/chats`.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc
        .chats
        .create_chat(
            &ctx,
            crate::domain::chat_service::CreateChatInput {
                title: body.title,
                model: body.model,
            },
        )
        .await?;
    let id = chat.id.to_string();
    Ok(created_json(chat, &uri, &id))
}

/// `GET /v1/chats`.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    OData(query): OData,
) -> ApiResult<impl IntoResponse> {
    let page = svc.chats.list_chats(&ctx, &query).await?;
    Ok(Json(page))
}

/// `GET /v1/chats/{id}`.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc.chats.get_chat(&ctx, id).await?;
    Ok(Json(chat))
}

/// `PATCH /v1/chats/{id}`.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc.chats.update_chat(&ctx, id, body.title).await?;
    Ok(Json(chat))
}

/// `DELETE /v1/chats/{id}`.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.chats.delete_chat(&ctx, id).await?;
    Ok(no_content())
}

/// `GET /v1/chats/{id}/messages`.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<impl IntoResponse> {
    let page = svc.chats.list_messages(&ctx, id, &query).await?;
    Ok(Json(page))
}

/// Cancels the turn's token when the SSE stream is dropped.
struct CancelOnDrop(tokio_util::sync::CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// SSE wiring shared by stream/retry/edit.
fn sse_response(
    setup: StreamSetup,
    ping_interval: std::time::Duration,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let events: std::pin::Pin<Box<dyn futures::Stream<Item = MiniChatSseEvent> + Send>> =
        match setup {
            StreamSetup::Live {
                receiver, cancel, ..
            } => {
                let relay = ping_relay(receiver, ping_interval);
                // The guard lives in the stream state; dropping the stream
                // (client disconnect) cancels the provider task.
                let relay = Box::pin(relay);
                Box::pin(futures::stream::unfold(
                    (relay, CancelOnDrop(cancel)),
                    |(mut relay, guard)| async move {
                        let next = relay.next().await;
                        next.map(|ev| (ev, (relay, guard)))
                    },
                ))
            }
            StreamSetup::Replay(events) => Box::pin(futures::stream::iter(events)),
        };
    Sse::new(events.map(|ev| Ok::<_, Infallible>(to_axum_event(&ev)))).keep_alive(
        KeepAlive::new().interval(std::time::Duration::from_secs(30)),
    )
}

/// The ping relay: `ping` events only between `stream_started` and the
/// first content event, every `interval` of idle time.
fn ping_relay(
    rx: crate::domain::stream_service::SseReceiver,
    interval: std::time::Duration,
) -> impl futures::Stream<Item = MiniChatSseEvent> + Send {
    futures::stream::unfold((rx, false, interval), |(mut rx, mut content_started, interval)| async move {
        if content_started {
            return rx.recv().await.map(|ev| (ev, (rx, true, interval)));
        }
        match tokio::time::timeout(interval, rx.recv()).await {
            Ok(Some(ev)) => {
                if matches!(
                    ev,
                    MiniChatSseEvent::Delta(_) | MiniChatSseEvent::Tool(_)
                ) {
                    content_started = true;
                }
                Some((ev, (rx, content_started, interval)))
            }
            Ok(None) => None,
            Err(_) => Some((MiniChatSseEvent::Ping(PingData), (rx, false, interval))),
        }
    })
}

/// Map an internal SSE event to an axum SSE event.
fn to_axum_event(ev: &MiniChatSseEvent) -> Event {
    let (name, payload) = match ev {
        MiniChatSseEvent::StreamStarted(data) => ("stream_started", serde_json::to_vec(data)),
        MiniChatSseEvent::Ping(data) => ("ping", serde_json::to_vec(data)),
        MiniChatSseEvent::Delta(data) => ("delta", serde_json::to_vec(data)),
        MiniChatSseEvent::Tool(data) => ("tool", serde_json::to_vec(data)),
        MiniChatSseEvent::Citations(data) => ("citations", serde_json::to_vec(data)),
        MiniChatSseEvent::Done(data) => ("done", serde_json::to_vec(data)),
        MiniChatSseEvent::Error(data) => ("error", serde_json::to_vec(data)),
    };
    match payload {
        Ok(bytes) => Event::default().event(name).data(String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => Event::default().event("error").data("{\"code\":\"internal\",\"message\":\"serialization failed\"}"),
    }
}

/// `POST /v1/chats/{id}/messages:stream`.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    extract::Json(body): extract::Json<StreamMessageRequest>,
) -> Result<impl IntoResponse, CanonicalError> {
    // The setup runs in a separate spawned task so a client disconnect
    // during setup does not interrupt it.
    let svc2 = Arc::clone(&svc);
    let setup = tokio::spawn(async move { svc2.stream.send_message(&ctx, id, body).await });
    let setup = match setup.await {
        Ok(result) => result?,
        Err(err) => {
            tracing::error!("mini-chat: stream setup task failed: {err}");
            return Err(DomainError::Internal.into());
        }
    };
    Ok(sse_response(setup, svc.ping_interval()))
}

/// `POST /v1/chats/{id}/turns/{request_id}/retry`.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, CanonicalError> {
    let svc2 = Arc::clone(&svc);
    let setup = tokio::spawn(async move { svc2.turns.retry_turn(&ctx, id, request_id).await });
    let setup = match setup.await {
        Ok(result) => result?,
        Err(err) => {
            tracing::error!("mini-chat: retry setup task failed: {err}");
            return Err(DomainError::Internal.into());
        }
    };
    Ok(sse_response(setup, svc.ping_interval()))
}

/// `PATCH /v1/chats/{id}/turns/{request_id}`.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> Result<impl IntoResponse, CanonicalError> {
    let svc2 = Arc::clone(&svc);
    let setup =
        tokio::spawn(async move { svc2.turns.edit_turn(&ctx, id, request_id, body.content).await });
    let setup = match setup.await {
        Ok(result) => result?,
        Err(err) => {
            tracing::error!("mini-chat: edit setup task failed: {err}");
            return Err(DomainError::Internal.into());
        }
    };
    Ok(sse_response(setup, svc.ping_interval()))
}

/// `GET /v1/chats/{id}/turns/{request_id}`.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    let turn = svc.turns.get_turn(&ctx, id, request_id).await?;
    Ok(Json(turn))
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.turns.delete_turn(&ctx, id, request_id).await?;
    Ok(no_content())
}

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<impl IntoResponse> {
    let reaction = svc.chats.set_reaction(&ctx, id, msg_id, &body.reaction).await?;
    Ok(Json(reaction))
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.chats.delete_reaction(&ctx, id, msg_id).await?;
    Ok(no_content())
}

/// `GET /v1/models`.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<impl IntoResponse> {
    let models = svc.models.list_models(&ctx).await?;
    Ok(Json(models))
}

/// `GET /v1/models/{id}`.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let model = svc.models.get_model(&ctx, &id).await?;
    Ok(Json(model))
}

/// `GET /v1/quota/status`.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<impl IntoResponse> {
    let status = svc.models.quota_status(&ctx).await?;
    Ok(Json(status))
}

/// `POST /v1/chats/{id}/attachments` (multipart upload, streaming body with
/// incremental byte counting).
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type).map_err(|_| DomainError::Multipart {
        field: "content_type",
        reason: "BOUNDARY_REQUIRED",
    })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    // Scan to the `file` field.
    let field = loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|_| DomainError::Multipart {
                field: "multipart",
                reason: "MULTIPART_ERROR",
            })?;
        let Some(field) = field else {
            return Err(DomainError::Multipart {
                field: "file",
                reason: "MISSING_FILE",
            }
            .into());
        };
        if field.name() == Some("file") {
            break field;
        }
    };
    let declared_mime = field
        .content_type()
        .map(std::string::ToString::to_string)
        .ok_or(DomainError::Multipart {
            field: "content_type",
            reason: "MISSING_CONTENT_TYPE",
        })?;
    let filename = normalize_filename(field.file_name());
    let mime = if declared_mime == "application/octet-stream" {
        crate::domain::attachment_service::mime_from_extension(&filename)
            .unwrap_or("application/octet-stream")
            .to_owned()
    } else {
        declared_mime
    };
    let plan = svc.attachments.prepare_upload(&ctx, id, &mime).await?;
    // Stream the body with incremental byte counting.
    let mut field = field;
    let mut data: Vec<u8> = Vec::new();
    let max = usize::try_from(plan.max_bytes).unwrap_or(usize::MAX);
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| DomainError::Multipart {
            field: "multipart",
            reason: "MULTIPART_ERROR",
        })?
    {
        if data.len() + chunk.len() > max {
            return Err(DomainError::FileTooLarge.into());
        }
        data.extend_from_slice(&chunk);
    }
    // CSV remap: stored as text/plain.
    let mime = if mime == "text/csv" { "text/plain".to_owned() } else { mime };
    let row = svc
        .attachments
        .upload(
            &ctx,
            plan,
            UploadInput {
                filename,
                content_type: mime,
                bytes: bytes::Bytes::from(data),
            },
        )
        .await?;
    Ok((
        http::StatusCode::CREATED,
        Json(crate::domain::attachment_service::attachment_detail(&row)),
    ))
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}`.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    let attachment = svc.attachments.get(&ctx, id, attachment_id).await?;
    Ok(Json(attachment))
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.attachments.delete(&ctx, id, attachment_id).await?;
    Ok(no_content())
}

/// Normalize the uploaded filename: default `upload`, truncated to 255
/// characters keeping the extension.
fn normalize_filename(filename: Option<&str>) -> String {
    let Some(name) = filename.filter(|f| !f.is_empty()) else {
        return "upload".to_owned();
    };
    let char_count = name.chars().count();
    if char_count <= 255 {
        return name.to_owned();
    }
    // Keep the extension.
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let ext_with_dot = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    let keep = 255 - ext_with_dot.chars().count();
    let stem: String = stem.chars().take(keep).collect();
    format!("{stem}{ext_with_dot}")
}
