//! Reaction DTOs (`SetReactionReq`, `MiniChatReactionDto`, `ReactionKindDto`).

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::reaction_service::{Reaction, ReactionView};

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

/// Request DTO for setting a reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    // A string, not `ReactionKindDto`: an unknown value is a 400 `INVALID_REACTION` of the
    // service, not a body-parse failure.
    pub reaction: String,
}

/// Response DTO for a reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "crate::api::dto::timestamp")]
    pub created_at: OffsetDateTime,
}

impl From<Reaction> for ReactionKindDto {
    fn from(r: Reaction) -> Self {
        match r {
            Reaction::Like => Self::Like,
            Reaction::Dislike => Self::Dislike,
        }
    }
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(r: ReactionView) -> Self {
        Self {
            message_id: r.message_id,
            reaction: r.reaction.into(),
            created_at: r.created_at,
        }
    }
}
