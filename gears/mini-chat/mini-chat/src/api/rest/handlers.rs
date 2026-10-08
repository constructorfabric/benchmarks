//! Axum handlers (non-streaming endpoints).

use std::sync::Arc;

use axum::Extension;
use axum::http::{HeaderMap, HeaderValue, Uri, header};
use axum::response::Response;
use base64::Engine;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, AttachmentKindDto, AttachmentStatusDto, AttachmentSummaryDto,
    ChatDetailDto, CreateChatReq, ImgThumbnailDto, MessageDto, MessageRoleDto, ModelDto,
    ModelListDto, ModelTierDto, QuotaPeriod, QuotaPeriodStatus, QuotaStatusResponse, QuotaTier,
    QuotaTierStatus, ReactionDto, ReactionKindDto, SetReactionReq, TurnStatusResponse,
    TurnStatusState, UpdateChatReq,
};
use crate::domain::app::AppServices;
use crate::domain::chats::{ChatView, ListError};
use crate::domain::messages::MessageView;
use crate::domain::quota::TierStatus;
use crate::infra::db::entities::{attachment, chat_turn};

/// Shared state injected into handlers.
pub type Svc = Arc<AppServices>;

pub(crate) fn list_error(e: ListError) -> CanonicalError {
    match e {
        ListError::Domain(d) => d.into(),
        ListError::OData(o) => CanonicalError::from(o),
    }
}

pub(crate) fn chat_dto(v: ChatView) -> ChatDetailDto {
    ChatDetailDto {
        id: v.chat.id,
        model: v.chat.model,
        title: v.chat.title,
        is_temporary: v.chat.is_temporary,
        message_count: v.message_count,
        created_at: v.chat.created_at,
        updated_at: v.chat.updated_at,
    }
}

pub(crate) fn model_dto(m: ModelCatalogEntry) -> ModelDto {
    ModelDto {
        model_id: m.id,
        display_name: m.display_name,
        tier: match m.tier {
            ModelTier::Premium => ModelTierDto::Premium,
            ModelTier::Standard => ModelTierDto::Standard,
        },
        multiplier_display: m.multiplier_display,
        description: if m.description.is_empty() {
            None
        } else {
            Some(m.description)
        },
        multimodal_capabilities: m.multimodal_capabilities,
        context_window: m.context_window,
    }
}

pub(crate) fn kind_dto(kind: &str) -> AttachmentKindDto {
    if kind == "image" {
        AttachmentKindDto::Image
    } else {
        AttachmentKindDto::Document
    }
}

pub(crate) fn status_dto(status: &str) -> AttachmentStatusDto {
    match status {
        "uploaded" => AttachmentStatusDto::Uploaded,
        "ready" => AttachmentStatusDto::Ready,
        "failed" => AttachmentStatusDto::Failed,
        _ => AttachmentStatusDto::Pending,
    }
}

pub(crate) fn thumbnail_dto(a: &attachment::Model) -> Option<ImgThumbnailDto> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let bytes = a.img_thumbnail.as_ref()?;
    Some(ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub(crate) fn attachment_detail_dto(a: &attachment::Model) -> AttachmentDetailDto {
    AttachmentDetailDto {
        id: a.id,
        filename: a.filename.clone(),
        content_type: a.content_type.clone(),
        size_bytes: a.size_bytes,
        status: status_dto(&a.status),
        kind: kind_dto(&a.attachment_kind),
        error_code: if a.status == "failed" {
            a.error_code.clone()
        } else {
            None
        },
        doc_summary: None,
        img_thumbnail: thumbnail_dto(a),
        summary_updated_at: None,
        created_at: a.created_at,
    }
}

fn message_dto(v: MessageView) -> Result<MessageDto, CanonicalError> {
    let m = v.message;
    let request_id = m
        .request_id
        .ok_or_else(|| CanonicalError::internal("stored message without request_id").create())?;
    let role = match m.role.as_str() {
        "assistant" => MessageRoleDto::Assistant,
        "system" => MessageRoleDto::System,
        _ => MessageRoleDto::User,
    };
    let my_reaction = match v.my_reaction.as_deref() {
        Some("like") => Some(ReactionKindDto::Like),
        Some("dislike") => Some(ReactionKindDto::Dislike),
        _ => None,
    };
    let attachments = v
        .attachments
        .iter()
        .map(|a| AttachmentSummaryDto {
            attachment_id: a.id,
            kind: kind_dto(&a.attachment_kind),
            filename: a.filename.clone(),
            status: status_dto(&a.status),
            img_thumbnail: thumbnail_dto(a),
        })
        .collect();
    let is_assistant = role == MessageRoleDto::Assistant;
    Ok(MessageDto {
        id: m.id,
        request_id,
        role,
        content: m.content,
        attachments,
        my_reaction,
        model: if is_assistant { m.model } else { None },
        input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
        output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
        created_at: m.created_at,
    })
}

pub(crate) fn turn_dto(t: &chat_turn::Model) -> TurnStatusResponse {
    let state = match t.state.as_str() {
        "completed" => TurnStatusState::Done,
        "failed" => TurnStatusState::Error,
        "cancelled" => TurnStatusState::Cancelled,
        _ => TurnStatusState::Running,
    };
    TurnStatusResponse {
        request_id: t.request_id,
        state,
        error_code: if state == TurnStatusState::Error {
            t.error_code.clone()
        } else {
            None
        },
        assistant_message_id: match state {
            TurnStatusState::Done | TurnStatusState::Cancelled => t.assistant_message_id,
            _ => None,
        },
        updated_at: t.updated_at,
    }
}

fn quota_dto(tiers: Vec<TierStatus>, threshold: u8) -> QuotaStatusResponse {
    QuotaStatusResponse {
        tiers: tiers
            .into_iter()
            .map(|t| QuotaTierStatus {
                tier: if t.tier == "premium" {
                    QuotaTier::Premium
                } else {
                    QuotaTier::Total
                },
                periods: t
                    .periods
                    .into_iter()
                    .map(|p| QuotaPeriodStatus {
                        period: if p.period == "daily" {
                            QuotaPeriod::Daily
                        } else {
                            QuotaPeriod::Monthly
                        },
                        limit_credits_micro: p.limit,
                        used_credits_micro: p.used,
                        remaining_credits_micro: p.remaining,
                        remaining_percentage: p.remaining_pct,
                        next_reset: p.next_reset,
                        warning: p.warning,
                        exhausted: p.exhausted,
                    })
                    .collect(),
            })
            .collect(),
        warning_threshold_pct: u32::from(threshold),
    }
}

// ───────────────────────────── chats ─────────────────────────────

pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = svc.create_chat(&ctx, req.title, req.model).await?;
    let id = view.chat.id;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), id);
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&location) {
        headers.insert(header::LOCATION, v);
    }
    Ok((StatusCode::CREATED, headers, Json(chat_dto(view))).into_response())
}

pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    OData(query): OData,
) -> ApiResult<Json<toolkit_odata::Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, &query).await.map_err(list_error)?;
    Ok(Json(page.map_items(chat_dto)))
}

pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(chat_dto(svc.get_chat(&ctx, id).await?)))
}

pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(chat_dto(svc.update_chat(&ctx, id, &req.title).await?)))
}

pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    svc.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── messages / reactions / turns ─────────────────────────────

pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<toolkit_odata::Page<MessageDto>>> {
    let page = svc
        .list_messages(&ctx, id, &query)
        .await
        .map_err(list_error)?;
    let mut items = Vec::with_capacity(page.items.len());
    for v in page.items {
        items.push(message_dto(v)?);
    }
    Ok(Json(toolkit_odata::Page::new(items, page.page_info)))
}

pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<ReactionDto>> {
    let (message_id, reaction, created_at) =
        svc.set_reaction(&ctx, id, msg_id, &req.reaction).await?;
    Ok(Json(ReactionDto {
        message_id,
        reaction: if reaction == "like" {
            ReactionKindDto::Like
        } else {
            ReactionKindDto::Dislike
        },
        created_at,
    }))
}

pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let turn = svc.get_turn(&ctx, id, request_id).await?;
    Ok(Json(turn_dto(&turn)))
}

// ───────────────────────────── models / quota ─────────────────────────────

pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
) -> ApiResult<Json<ModelListDto>> {
    let items = svc
        .list_models(&ctx)
        .await?
        .into_iter()
        .map(model_dto)
        .collect();
    Ok(Json(ModelListDto { items }))
}

pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    Ok(Json(model_dto(svc.get_model(&ctx, &id).await?)))
}

pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let tiers = svc.quota_status(&ctx).await?;
    Ok(Json(quota_dto(tiers, svc.cfg.quota.warning_threshold_pct)))
}

// ───────────────────────────── attachments ─────────────────────────────

pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let a = svc.get_attachment(&ctx, id, attachment_id).await?;
    Ok(Json(attachment_detail_dto(&a)))
}

pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
