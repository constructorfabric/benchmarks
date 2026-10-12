//! Row builders shared by the repositories that insert user messages.

use sea_orm::ActiveValue;
use time::OffsetDateTime;
use uuid::Uuid;

use super::entity::messages;

/// A stored `user` message of a turn.
pub(super) fn user_message(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    content: String,
    now: OffsetDateTime,
) -> messages::ActiveModel {
    messages::ActiveModel {
        id: ActiveValue::Set(id),
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(Some(request_id)),
        role: ActiveValue::Set("user".to_owned()),
        content: ActiveValue::Set(content),
        content_type: ActiveValue::Set("text".to_owned()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(None),
        request_kind: ActiveValue::Set("chat".to_owned()),
        features_used: ActiveValue::Set(serde_json::json!([])),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        cache_read_input_tokens: ActiveValue::Set(0),
        cache_write_input_tokens: ActiveValue::Set(0),
        reasoning_tokens: ActiveValue::Set(0),
        model: ActiveValue::Set(None),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(now),
        deleted_at: ActiveValue::Set(None),
    }
}

/// The `created_at` of the link at `position`: one microsecond apart, so that the links of a
/// message keep the order of the request's `attachment_ids`.
pub(super) fn link_time(now: OffsetDateTime, position: usize) -> OffsetDateTime {
    now + time::Duration::microseconds(i64::try_from(position).unwrap_or(0))
}
