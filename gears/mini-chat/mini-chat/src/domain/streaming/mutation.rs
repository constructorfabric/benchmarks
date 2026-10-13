//! The stream-service half of a retry / edit (DESIGN 3.6 "Retry / edit
//! variant", 3.9 rule 8): the quota preflight that runs before the mutation
//! commits, and, after the commit, context assembly, provider resolution, the
//! reserve and the launch of the same provider task a send uses. There is no
//! separate settlement or outbox path: the launched turn is finalized by
//! [`super::finalize`].

use std::sync::Arc;

use toolkit_security::SecurityContext;

use super::reserve::AdmittedTurn;
use super::{
    MutationPlan, MutationRequest, SendMessageCommand, StartedTurn, StreamService, TurnEvents,
    TurnScopes,
};
use crate::domain::chats::ChatRow;
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::TurnRepository;
use crate::domain::turns::{TurnFinalization, TurnState};

/// `chat_turns.error_code` of a retry / edit whose context does not fit.
const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";

/// `chat_turns.error_code` of a retry / edit whose reserve re-check failed.
const QUOTA_EXCEEDED: &str = "quota_exceeded";

/// `chat_turns.error_code` of any other setup failure of a retry / edit.
const TURN_SETUP_FAILED: &str = "turn_setup_failed";

/// The `error_code` of the new turn for a setup failure.
fn setup_failure_code(error: &DomainError) -> &'static str {
    match error {
        DomainError::OutOfRange { .. } => CONTEXT_LENGTH_EXCEEDED,
        DomainError::QuotaExceeded { .. } => QUOTA_EXCEEDED,
        _ => TURN_SETUP_FAILED,
    }
}

impl StreamService {
    pub(super) async fn mutation_gate(
        &self,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        request: MutationRequest,
    ) -> Result<MutationPlan, DomainError> {
        let command = SendMessageCommand {
            content: request.content,
            request_id: None,
            attachment_ids: request.attachment_ids,
            web_search: request.web_search,
        };
        let conn = self.db.conn()?;
        let gate = self
            .preflight_gate(
                &conn,
                ctx,
                scopes,
                chat,
                &command,
                Some(request.replaced_request),
            )
            .await?;
        Ok(MutationPlan { gate, command })
    }

    pub(super) async fn resume_started_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        scopes: TurnScopes,
        chat: ChatRow,
        started: StartedTurn,
        plan: MutationPlan,
    ) -> Result<TurnEvents, DomainError> {
        let MutationPlan { gate, command } = plan;
        let turn_plan = match self.db.conn() {
            Ok(conn) => {
                self.preflight_plan(
                    &conn,
                    ctx,
                    &scopes,
                    &chat,
                    &command,
                    gate,
                    Some(started.request_id),
                )
                .await
            }
            Err(error) => Err(error),
        };
        let turn_plan = match turn_plan {
            Ok(turn_plan) => turn_plan,
            Err(error) => return Err(self.fail_unstarted(&scopes, &started, error).await),
        };
        if let Err(error) = self
            .reserve_started(ctx, &scopes, &chat, started.turn_id, &turn_plan)
            .await
        {
            return Err(self.fail_unstarted(&scopes, &started, error).await);
        }
        Ok(self.launch(
            ctx,
            scopes,
            AdmittedTurn {
                plan: turn_plan,
                chat,
                request_id: started.request_id,
                turn_id: started.turn_id,
                assistant_message_id: uuid::Uuid::new_v4(),
                user_created_at: started.user_created_at,
            },
        ))
    }

    /// Moves the new turn to `failed` with a plain CAS (no reserve exists, so
    /// no settlement and no event; DESIGN 5.7 "Unstarted retry/edit turn") and
    /// hands the error back to be returned to the client. A turn that cannot
    /// be failed is left to the orphan watchdog.
    async fn fail_unstarted(
        &self,
        scopes: &TurnScopes,
        started: &StartedTurn,
        error: DomainError,
    ) -> DomainError {
        let finalization = TurnFinalization {
            turn_id: started.turn_id,
            state: TurnState::Failed,
            error_code: Some(setup_failure_code(&error).to_owned()),
            assistant_message_id: None,
            provider_response_id: None,
            now: now_utc(),
        };
        let failed = match self.db.conn() {
            Ok(conn) => conn.finalize_turn(&scopes.tenant, &finalization).await,
            Err(error) => Err(error),
        };
        if let Err(cause) = failed {
            tracing::error!(turn_id = %started.turn_id, error = %cause,
                "the unstarted retry/edit turn could not be failed; the watchdog will");
        }
        error
    }
}
