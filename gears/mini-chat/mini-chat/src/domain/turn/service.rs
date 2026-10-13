//! The turn status service.

use std::sync::Arc;

use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::model::TurnStatus;
use crate::domain::authz::{Pep, actions};
use crate::domain::chat::ChatRepository;
use crate::domain::error::DomainError;
use crate::domain::stream::TurnRepository;

/// `GET /v1/chats/{id}/turns/{request_id}`: authoritative turn state backed by `chat_turns`.
#[domain_model]
pub struct TurnService {
    pep: Pep,
    chats: Arc<dyn ChatRepository>,
    turns: Arc<dyn TurnRepository>,
}

impl TurnService {
    /// Creates the service.
    #[must_use]
    pub fn new(pep: Pep, chats: Arc<dyn ChatRepository>, turns: Arc<dyn TurnRepository>) -> Self {
        Self { pep, chats, turns }
    }

    /// The status of one turn of the caller's chat. A soft-deleted turn, a turn of another chat
    /// and a foreign chat are all `not found`.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `ChatNotFound`, `TurnNotFound` or an internal error.
    pub async fn status(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnStatus, DomainError> {
        let scope = self
            .pep
            .chat_scope(ctx, actions::READ_TURN, Some(chat_id))
            .await?;
        self.chats
            .find(&scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound { chat_id })?;
        let turn = self
            .turns
            .find_by_request(&scope, chat_id, request_id)
            .await?
            .filter(|turn| !turn.deleted)
            .ok_or(DomainError::TurnNotFound { request_id })?;
        Ok(TurnStatus::from(turn))
    }
}
