//! Imports shared by the handler modules.

pub(super) use std::sync::Arc;

pub(super) use axum::Extension;
pub(super) use axum::body::Body;
pub(super) use axum::http::{HeaderMap, StatusCode, Uri, header};
pub(super) use axum::response::{IntoResponse, Response};
pub(super) use bytes::{Bytes, BytesMut};
pub(super) use toolkit::api::canonical_prelude::*;
pub(super) use toolkit::api::odata::OData;
pub(super) use toolkit::api::rest::extract;
pub(super) use toolkit::{Page, PageInfo};
pub(super) use toolkit_security::SecurityContext;
pub(super) use uuid::Uuid;

pub(super) use crate::api::rest::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto, ModelDto,
    ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
pub(super) use crate::domain::error::{DomainError, Res};
pub(super) use crate::domain::service::MiniChatService;
pub(super) use crate::domain::service::attachments::normalize_filename;
pub(super) use crate::domain::service::stream::SendInput;

pub(super) use crate::api::rest::sse::detached;

pub(super) type Svc = Extension<Arc<MiniChatService>>;

pub(super) fn err(e: DomainError) -> CanonicalError {
    e.into()
}
