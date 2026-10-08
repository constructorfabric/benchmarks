//! Message listing (OWNER: REST CRUD work package).
//!
//! `GET /v1/chats/{id}/messages` with OData paging and batch enrichment
//! (attachments, `my_reaction`).

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use toolkit_db::odata::{FieldToColumn, ODataFieldMapping, paginate_odata};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::dto::{
    AttachmentKindDto, AttachmentStatusDto, AttachmentSummaryDto, ImgThumbnailDto, MessageRoleDto,
    MiniChatMessageDto, MiniChatMessageDtoFilterField, ReactionKindDto,
};
use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::service::Deps;
use crate::domain::service::chat_access::load_chat;
use crate::domain::service::chats::LIST_LIMITS;
use crate::infra::db::entity::{attachment, message, message_attachment, message_reaction};

/// Content type of stored thumbnails.
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// `$filter` / `$orderby` mapping of the message list.
pub struct MessageODataMapper;

impl FieldToColumn<MiniChatMessageDtoFilterField> for MessageODataMapper {
    type Column = message::Column;

    fn map_field(field: MiniChatMessageDtoFilterField) -> message::Column {
        match field {
            MiniChatMessageDtoFilterField::Id => message::Column::Id,
            MiniChatMessageDtoFilterField::Role => message::Column::Role,
            MiniChatMessageDtoFilterField::CreatedAt => message::Column::CreatedAt,
        }
    }

    fn map_value(
        field: MiniChatMessageDtoFilterField,
        _op: toolkit_odata::filter::FilterOp,
        value: &toolkit_odata::filter::ODataValue,
    ) -> Result<toolkit_odata::filter::ODataValue, String> {
        if matches!(field, MiniChatMessageDtoFilterField::Id) {
            return crate::domain::service::chats::map_uuid_value(value);
        }
        Ok(value.clone())
    }
}

impl ODataFieldMapping<MiniChatMessageDtoFilterField> for MessageODataMapper {
    type Entity = message::Entity;

    fn cursor_kind(field: MiniChatMessageDtoFilterField) -> toolkit_odata::filter::FieldKind {
        match field {
            MiniChatMessageDtoFilterField::Id => toolkit_odata::filter::FieldKind::Uuid,
            other => toolkit_odata::filter::FilterField::kind(&other),
        }
    }

    fn extract_cursor_value(
        m: &message::Model,
        field: MiniChatMessageDtoFilterField,
    ) -> sea_orm::Value {
        match field {
            MiniChatMessageDtoFilterField::Id => sea_orm::Value::Uuid(Some(m.id)),
            MiniChatMessageDtoFilterField::Role => sea_orm::Value::String(Some(m.role.clone())),
            MiniChatMessageDtoFilterField::CreatedAt => {
                sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.created_at))
            }
        }
    }
}

/// Maps a stored role.
///
/// # Errors
/// `Internal` on an unknown value.
pub fn role_dto(role: &str) -> Result<MessageRoleDto, DomainError> {
    match role {
        "user" => Ok(MessageRoleDto::User),
        "assistant" => Ok(MessageRoleDto::Assistant),
        "system" => Ok(MessageRoleDto::System),
        other => Err(DomainError::internal(format!(
            "unknown message role '{other}'"
        ))),
    }
}

/// Maps a stored reaction value.
///
/// # Errors
/// `Internal` on an unknown value.
pub fn reaction_dto(reaction: &str) -> Result<ReactionKindDto, DomainError> {
    match reaction {
        "like" => Ok(ReactionKindDto::Like),
        "dislike" => Ok(ReactionKindDto::Dislike),
        other => Err(DomainError::internal(format!("unknown reaction '{other}'"))),
    }
}

fn kind_dto(kind: &str) -> Result<AttachmentKindDto, DomainError> {
    match kind {
        "document" => Ok(AttachmentKindDto::Document),
        "image" => Ok(AttachmentKindDto::Image),
        other => Err(DomainError::internal(format!(
            "unknown attachment kind '{other}'"
        ))),
    }
}

fn status_dto(status: &str) -> Result<AttachmentStatusDto, DomainError> {
    match status {
        "pending" => Ok(AttachmentStatusDto::Pending),
        "uploaded" => Ok(AttachmentStatusDto::Uploaded),
        "ready" => Ok(AttachmentStatusDto::Ready),
        "failed" => Ok(AttachmentStatusDto::Failed),
        other => Err(DomainError::internal(format!(
            "unknown attachment status '{other}'"
        ))),
    }
}

/// Thumbnail of a ready image attachment (`None` for documents, non-ready rows or no bytes).
#[must_use]
pub fn thumbnail_dto(a: &attachment::Model) -> Option<ImgThumbnailDto> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let bytes = a.img_thumbnail.as_ref().filter(|b| !b.is_empty())?;
    Some(ImgThumbnailDto {
        content_type: THUMBNAIL_CONTENT_TYPE.to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// Summary of one attachment embedded in a message.
///
/// # Errors
/// `Internal` on unknown stored enum values.
pub fn attachment_summary(a: &attachment::Model) -> Result<AttachmentSummaryDto, DomainError> {
    Ok(AttachmentSummaryDto {
        attachment_id: a.id,
        kind: kind_dto(&a.attachment_kind)?,
        filename: a.filename.clone(),
        status: status_dto(&a.status)?,
        img_thumbnail: thumbnail_dto(a),
    })
}

/// Batch-loads the non-deleted attachments linked to `message_ids` of `chat_id`.
async fn load_attachments(
    runner: &impl DBRunner,
    child_scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<AttachmentSummaryDto>>, DomainError> {
    let mut out: HashMap<Uuid, Vec<AttachmentSummaryDto>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::ChatId.eq(chat_id))
        .filter(message_attachment::Column::MessageId.is_in(message_ids.iter().copied()))
        .order_by_asc(message_attachment::Column::CreatedAt)
        .order_by_asc(message_attachment::Column::AttachmentId)
        .secure()
        .scope_with(child_scope)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(out);
    }
    let mut att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    att_ids.sort_unstable();
    att_ids.dedup();
    let atts: HashMap<Uuid, attachment::Model> = attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Id.is_in(att_ids))
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(child_scope)
        .all(runner)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    for link in links {
        if let Some(a) = atts.get(&link.attachment_id) {
            out.entry(link.message_id)
                .or_default()
                .push(attachment_summary(a)?);
        }
    }
    Ok(out)
}

/// Batch-loads the caller's reactions on `message_ids`.
async fn load_reactions(
    runner: &impl DBRunner,
    child_scope: &AccessScope,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, ReactionKindDto>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = child_scope.ensure_owner(user_id);
    let rows = message_reaction::Entity::find()
        .filter(message_reaction::Column::MessageId.is_in(message_ids.iter().copied()))
        .filter(message_reaction::Column::UserId.eq(user_id))
        .secure()
        .scope_with(&scope)
        .all(runner)
        .await?;
    rows.into_iter()
        .map(|r| Ok((r.message_id, reaction_dto(&r.reaction)?)))
        .collect()
}

/// Builds the wire message.
///
/// # Errors
/// `Internal` when `request_id` is NULL or a stored enum is unknown.
pub fn message_to_dto(
    m: message::Model,
    attachments: Vec<AttachmentSummaryDto>,
    my_reaction: Option<ReactionKindDto>,
) -> Result<MiniChatMessageDto, DomainError> {
    let request_id = m
        .request_id
        .ok_or_else(|| DomainError::internal(format!("message {} has no request_id", m.id)))?;
    let role = role_dto(&m.role)?;
    let my_reaction = if role == MessageRoleDto::Assistant {
        my_reaction
    } else {
        None
    };
    Ok(MiniChatMessageDto {
        id: m.id,
        request_id,
        role,
        content: m.content,
        attachments,
        my_reaction,
        model: m.model,
        input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
        output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
        created_at: m.created_at,
    })
}

pub struct MessageService {
    deps: Arc<Deps>,
}

impl MessageService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// `GET /v1/chats/{id}/messages`: non-deleted messages, default order `created_at asc, id asc`.
    ///
    /// # Errors
    /// 404 chat, 403/503 from the PEP, 400 OData errors, 500 on a NULL `request_id`.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        mut query: ODataQuery,
    ) -> Result<Page<MiniChatMessageDto>, DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, actions::LIST_MESSAGES).await?;
        if query.cursor.is_none() && query.order.is_empty() {
            query.order = ODataOrderBy(vec![OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            }]);
        }
        let conn = self.deps.db.conn()?;
        let select = message::Entity::find()
            .filter(message::Column::ChatId.eq(ac.chat.id))
            .filter(message::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&ac.child_scope);
        let page: Page<message::Model> =
            paginate_odata::<MiniChatMessageDtoFilterField, MessageODataMapper, _, _, _, _>(
                select,
                &conn,
                &query,
                ("id", SortDir::Asc),
                LIST_LIMITS,
                |m| m,
            )
            .await?;

        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let assistant_ids: Vec<Uuid> = page
            .items
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.id)
            .collect();
        let mut attachments = load_attachments(&conn, &ac.child_scope, ac.chat.id, &ids).await?;
        let reactions =
            load_reactions(&conn, &ac.child_scope, ctx.subject_id(), &assistant_ids).await?;

        let Page { items, page_info } = page;
        let items = items
            .into_iter()
            .map(|m| {
                let atts = attachments.remove(&m.id).unwrap_or_default();
                let reaction = reactions.get(&m.id).copied();
                message_to_dto(m, atts, reaction)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page { items, page_info })
    }
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod messages_tests;
