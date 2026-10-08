//! Axum handlers. Pre-stream failures are canonical `Problem` responses;
//! after the SSE stream opened failures are terminal `error` events.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use tokio_util::sync::DropGuard;
use toolkit::api::canonical_prelude::{ApiResult, CanonicalError, OData, created_json, no_content, ok_json};
use toolkit::api::rest::extract;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto, ModelDto, ModelListDto,
    QuotaStatusResponse, ReactionDto, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use crate::domain::attachments::normalize_filename;
use crate::domain::errors::{DomainError, Res};
use crate::domain::events::StreamEvent;
use crate::domain::state::AppState;
use crate::domain::stream::{SendInput, StreamStart};

type State = Extension<Arc<AppState>>;

fn err(e: DomainError) -> CanonicalError {
    e.into()
}

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = st.create_chat(&ctx, body.title, body.model).await.map_err(err)?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    OData(q): OData,
) -> ApiResult<impl IntoResponse> {
    let page = st.list_chats(&ctx, q).await.map_err(err)?;
    let out: Page<ChatDetailDto> = Page {
        items: page.items.into_iter().map(Into::into).collect(),
        page_info: page.page_info,
    };
    Ok(ok_json(out))
}

pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    Ok(ok_json(ChatDetailDto::from(st.get_chat(&ctx, id).await.map_err(err)?)))
}

pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<impl IntoResponse> {
    Ok(ok_json(ChatDetailDto::from(
        st.update_chat_title(&ctx, id, &body.title).await.map_err(err)?,
    )))
}

pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    st.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(no_content())
}

pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(id): extract::Path<Uuid>,
    OData(q): OData,
) -> ApiResult<impl IntoResponse> {
    let page = st.list_messages(&ctx, id, q).await.map_err(err)?;
    let out: Page<MessageDto> = Page {
        items: page.items.into_iter().map(Into::into).collect(),
        page_info: page.page_info,
    };
    Ok(ok_json(out))
}

pub async fn set_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<impl IntoResponse> {
    let r = st.set_reaction(&ctx, chat_id, msg_id, &body.reaction).await.map_err(err)?;
    Ok(ok_json(ReactionDto::from(r)))
}

pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    st.delete_reaction(&ctx, chat_id, msg_id).await.map_err(err)?;
    Ok(no_content())
}

pub async fn list_models(Extension(ctx): Extension<SecurityContext>, Extension(st): State) -> ApiResult<impl IntoResponse> {
    let items = st.list_models(&ctx).await.map_err(err)?;
    Ok(ok_json(ModelListDto {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<impl IntoResponse> {
    Ok(ok_json(ModelDto::from(st.get_model(&ctx, &id).await.map_err(err)?)))
}

pub async fn quota_status(Extension(ctx): Extension<SecurityContext>, Extension(st): State) -> ApiResult<impl IntoResponse> {
    let (statuses, threshold) = st.quota_status(&ctx).await.map_err(err)?;
    Ok(ok_json(QuotaStatusResponse::build(&statuses, threshold)))
}

pub async fn turn_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    Ok(ok_json(TurnStatusResponse::from(
        st.turn_status(&ctx, chat_id, request_id).await.map_err(err)?,
    )))
}

pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    st.delete_turn(&ctx, chat_id, request_id).await.map_err(err)?;
    Ok(no_content())
}

// ───────────────────────────── streaming ─────────────────────────────

fn to_event(ev: &StreamEvent) -> Event {
    Event::default()
        .event(ev.name())
        .data(serde_json::to_string(&ev.data()).unwrap_or_else(|_| "{}".to_owned()))
}

/// Live SSE body: relays events; dropping it (client disconnect) cancels
/// the turn's token.
struct LiveSse {
    rx: tokio::sync::mpsc::Receiver<StreamEvent>,
    _guard: DropGuard,
    done: bool,
}

impl Stream for LiveSse {
    type Item = Result<Event, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(ev)) => {
                if ev.is_terminal() {
                    self.done = true;
                }
                Poll::Ready(Some(Ok(to_event(&ev))))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn sse_response(start: StreamStart) -> Response {
    let keep_alive = KeepAlive::new().interval(Duration::from_secs(30));
    match start {
        StreamStart::Replay(events) => {
            let s = futures::stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(to_event(&e))));
            Sse::new(s).keep_alive(keep_alive).into_response()
        }
        StreamStart::Live(live) => {
            let s = LiveSse {
                rx: live.rx,
                _guard: live.cancel.drop_guard(),
                done: false,
            };
            Sse::new(s).keep_alive(keep_alive).into_response()
        }
    }
}

async fn run_setup<F>(fut: F) -> ApiResult<Response>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(Ok(start)) => Ok(sse_response(start)),
        Ok(Err(e)) => Err(err(e)),
        Err(e) => Err(err(DomainError::internal(format!("stream setup task failed: {e}")))),
    }
}

pub async fn send_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(chat_id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let input = SendInput {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    run_setup(async move { st.start_send(ctx, chat_id, input).await }).await
}

pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    run_setup(async move { st.retry_turn(ctx, chat_id, request_id).await }).await
}

pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    run_setup(async move { st.edit_turn(ctx, chat_id, request_id, body.content).await }).await
}

// ───────────────────────────── attachments ─────────────────────────────

fn mp_err(field: &str, reason: &str, desc: &str) -> CanonicalError {
    err(DomainError::field(Res::Attachment, field, reason, desc))
}

pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path(chat_id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    let up = st.upload_precheck(&ctx, chat_id).await.map_err(err)?;
    let Ok(_permit) = Arc::clone(&st.upload_slots).try_acquire_owned() else {
        return Err(err(DomainError::unavailable(5)));
    };
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(ct)
        .map_err(|_| mp_err("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => return Err(mp_err("file", "MISSING_FILE", "the 'file' field is required")),
            Err(_) => return Err(mp_err("multipart", "MULTIPART_ERROR", "the multipart body could not be read")),
        };
        if field.name() != Some("file") {
            continue;
        }
        let Some(declared) = field.content_type().map(ToString::to_string) else {
            return Err(mp_err("content_type", "MISSING_CONTENT_TYPE", "the file part has no content type"));
        };
        let filename = normalize_filename(field.file_name());
        let kind = st.classify_upload(&up, &declared, &filename).map_err(err)?;
        let mut buf = BytesMut::new();
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if (buf.len() + chunk.len()) as u64 > kind.max_bytes {
                        return Err(err(DomainError::out_of_range(
                            Res::Attachment,
                            "content_length",
                            "FILE_TOO_LARGE",
                            "the file exceeds the upload size limit",
                        )));
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Err(mp_err("multipart", "MULTIPART_ERROR", "the multipart body could not be read")),
            }
        }
        let data: Bytes = buf.freeze();
        let detail = st
            .upload_attachment(&ctx, up, filename, kind, data, started)
            .await
            .map_err(err)?;
        return Ok((StatusCode::CREATED, axum::Json(AttachmentDetailDto::from(detail))).into_response());
    }
}

pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    Ok(ok_json(AttachmentDetailDto::from(
        st.get_attachment(&ctx, chat_id, id).await.map_err(err)?,
    )))
}

pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): State,
    extract::Path((chat_id, id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    st.delete_attachment(&ctx, chat_id, id).await.map_err(err)?;
    Ok(no_content())
}

#[allow(dead_code)]
fn _assert_stream(s: impl StreamExt) -> impl StreamExt {
    s
}
