//! Turn status read API (OWNER: REST CRUD work package).
//!
//! `GET /v1/chats/{id}/turns/{request_id}` (DESIGN §3.3 Turn Status API).

use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{TurnStatusResponse, TurnStatusState};
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, resource_types};
use crate::domain::service::Deps;
use crate::domain::service::chat_access::load_chat;
use crate::infra::db::entity::chat_turn;

/// Maps the internal turn state to the API state.
///
/// # Errors
/// `Internal` on an unknown stored state.
pub fn state_dto(state: &str) -> Result<TurnStatusState, DomainError> {
    match state {
        "running" => Ok(TurnStatusState::Running),
        "completed" => Ok(TurnStatusState::Done),
        "failed" => Ok(TurnStatusState::Error),
        "cancelled" => Ok(TurnStatusState::Cancelled),
        other => Err(DomainError::internal(format!(
            "unknown turn state '{other}'"
        ))),
    }
}

/// Builds the wire turn status of a turn row.
///
/// # Errors
/// `Internal` on an unknown stored state.
pub fn turn_to_dto(t: &chat_turn::Model) -> Result<TurnStatusResponse, DomainError> {
    let state = state_dto(&t.state)?;
    Ok(TurnStatusResponse {
        request_id: t.request_id,
        state,
        error_code: if state == TurnStatusState::Error {
            t.error_code.clone()
        } else {
            None
        },
        assistant_message_id: t.assistant_message_id,
        updated_at: t.updated_at,
    })
}

pub struct TurnStatusService {
    deps: Arc<Deps>,
}

impl TurnStatusService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// Reads the non-deleted turn `request_id` of the chat.
    ///
    /// # Errors
    /// 404 chat (inaccessible) or turn (missing / soft-deleted), 403/503 from the PEP.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatusResponse, DomainError> {
        let ac = load_chat(&self.deps, ctx, chat_id, actions::READ_TURN).await?;
        let conn = self.deps.db.conn()?;
        let turn = chat_turn::Entity::find()
            .filter(chat_turn::Column::ChatId.eq(ac.chat.id))
            .filter(chat_turn::Column::RequestId.eq(request_id))
            .filter(chat_turn::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&ac.child_scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::NotFound {
                resource: resource_types::TURN,
            })?;
        turn_to_dto(&turn)
    }
}

#[cfg(test)]
#[path = "turn_status_tests.rs"]
mod turn_status_tests;
