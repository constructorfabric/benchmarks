//! Turn DTOs: status (`TurnStatusResponse`, `TurnStatusState`) and edit (`EditTurnRequest`).

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::turn_service::{TurnStatus, TurnStatusView};

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).

/// Turn state as reported by the turn status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    // Omitted unless the turn failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    // Omitted unless an assistant message was persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "crate::api::dto::timestamp")]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatus> for TurnStatusState {
    fn from(s: TurnStatus) -> Self {
        match s {
            TurnStatus::Running => Self::Running,
            TurnStatus::Done => Self::Done,
            TurnStatus::Error => Self::Error,
            TurnStatus::Cancelled => Self::Cancelled,
        }
    }
}

impl From<TurnStatusView> for TurnStatusResponse {
    fn from(t: TurnStatusView) -> Self {
        Self {
            request_id: t.request_id,
            state: t.state.into(),
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}
