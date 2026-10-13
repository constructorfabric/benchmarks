//! Mutation input loading
//! (`cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs`).

use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::MutationRepository;
use crate::domain::error::DomainError;
use crate::domain::service::stream::inputs::{InputLimits, TurnInputs, load_attachment_inputs};
use crate::domain::service::stream::ports::TurnRepository;

/// The inputs of a replacement before the commit.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MutationInputs {
    /// The carried attachment ids: the links of the old user message whose
    /// attachment row is not deleted, in link order.
    pub carried_ids: Vec<Uuid>,
    /// The attachment part of the turn inputs (images, flags, vector store,
    /// citation map); the history fields stay empty.
    pub inputs: TurnInputs,
}

/// Loads the carried attachment ids and the attachment part of the turn
/// inputs. The history steps are not run here: they run after the commit.
///
/// # Errors
///
/// `TooManyImages` and the errors of the repositories.
pub async fn load_mutation_inputs(
    turns: &dyn TurnRepository,
    mutations: &dyn MutationRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    user_message_id: Uuid,
    limits: &InputLimits,
) -> Result<MutationInputs, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-attachment-inputs
    let carried_ids = mutations
        .carried_attachment_ids(scope, chat_id, user_message_id)
        .await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-return-mutation-too-many-images
    let inputs = load_attachment_inputs(turns, scope, chat_id, &carried_ids, limits).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-return-mutation-too-many-images
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-attachment-inputs
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-return-mutation-inputs
    Ok(MutationInputs {
        carried_ids,
        inputs,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-mutation-inputs:p1:inst-return-mutation-inputs
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
