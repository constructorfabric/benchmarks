//! Turn input loading
//! (`cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`): every read of
//! the pipeline that does not lock, run before the reserve transaction.

use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::{
    AttachmentKind, CitationMap, CitationTarget, HistoryMessage, MessageBoundary, RecentQuery,
    ThreadSummaryInfo, TurnRepository,
};
use crate::domain::error::DomainError;

/// The limits of the input loading.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLimits {
    /// `context.recent_messages_limit`.
    pub recent_messages_limit: u32,
    /// `rag.max_images_per_message`.
    pub max_images_per_message: u32,
}

/// An image of the request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Primary provider file id.
    pub provider_file_id: Option<String>,
    /// Secondary provider file id.
    pub secondary_file_id: Option<String>,
}

/// What the pipeline reads before the preflight.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnInputs {
    /// The images among the request's `attachment_ids` (live rows of the
    /// chat of kind `image`), in the order of the request ids.
    pub images: Vec<ImageAttachment>,
    /// `images.len()`.
    pub image_count: u32,
    /// A ready attachment with `for_file_search`.
    pub has_ready_file_search_documents: bool,
    /// A ready attachment with `for_code_interpreter` and a provider file id.
    pub has_ready_code_interpreter_files: bool,
    /// Provider file ids of those attachments in `created_at`, `id` order.
    pub code_interpreter_file_ids: Vec<String>,
    /// The chat vector store id, when the row exists and the id is set.
    pub vector_store_id: Option<String>,
    /// Provider file id to attachment of every ready attachment.
    pub citation_map: CitationMap,
    /// The snapshot boundary.
    pub boundary: Option<MessageBoundary>,
    /// The thread summary.
    pub summary: Option<ThreadSummaryInfo>,
    /// Recent messages, chronological.
    pub recent: Vec<HistoryMessage>,
}

/// Loads the turn inputs; stops with `TOO_MANY_IMAGES` before the remaining
/// reads.
///
/// # Errors
///
/// [`DomainError::TooManyImages`] and the errors of the repository.
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-reads-before-tx
pub async fn load_turn_inputs(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    attachment_ids: &[Uuid],
    limits: &InputLimits,
) -> Result<TurnInputs, DomainError> {
    let mut inputs = load_attachment_inputs(repo, scope, chat_id, attachment_ids, limits).await?;
    inputs.boundary = repo.snapshot_boundary(scope, chat_id).await?;
    inputs.summary = repo.thread_summary(scope, chat_id).await?;
    if let Some(boundary) = inputs.boundary
        && limits.recent_messages_limit > 0
    {
        let query = RecentQuery {
            boundary,
            frontier: inputs.summary.as_ref().map(|summary| summary.frontier),
            limit: limits.recent_messages_limit,
        };
        inputs.recent = repo.recent_messages(scope, chat_id, &query).await?;
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-return-inputs
    Ok(inputs)
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-return-inputs
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-reads-before-tx

/// The attachment part of [`load_turn_inputs`]: the images among the request's
/// attachments, `TOO_MANY_IMAGES`, the ready-attachment flags, the vector
/// store and the citation map. The history fields of the result stay empty.
///
/// # Errors
///
/// [`DomainError::TooManyImages`] and the errors of the repository.
pub async fn load_attachment_inputs(
    repo: &dyn TurnRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    attachment_ids: &[Uuid],
    limits: &InputLimits,
) -> Result<TurnInputs, DomainError> {
    let mut inputs = TurnInputs::default();
    if !attachment_ids.is_empty() {
        let rows = repo
            .request_attachments(scope, chat_id, attachment_ids)
            .await?;
        for id in attachment_ids {
            if let Some(row) = rows
                .iter()
                .find(|row| row.id == *id && row.kind == AttachmentKind::Image)
            {
                inputs.images.push(ImageAttachment {
                    id: row.id,
                    provider_file_id: row.provider_file_id.clone(),
                    secondary_file_id: row.secondary_file_id.clone(),
                });
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-count-images
        inputs.image_count = u32::try_from(inputs.images.len()).unwrap_or(u32::MAX);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-count-images
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-return-too-many-images
        if inputs.image_count > limits.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-return-too-many-images
    }

    let mut ready = repo.ready_attachments(scope, chat_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-ready-flags
    inputs.has_ready_file_search_documents = ready.iter().any(|row| row.for_file_search);
    ready.sort_by_key(|row| (row.created_at, row.id));
    for row in &ready {
        if row.for_code_interpreter
            && let Some(file_id) = &row.provider_file_id
        {
            inputs.has_ready_code_interpreter_files = true;
            inputs.code_interpreter_file_ids.push(file_id.clone());
        }
        if let Some(file_id) = &row.provider_file_id {
            inputs.citation_map.insert(
                file_id.clone(),
                CitationTarget {
                    attachment_id: row.id,
                    filename: row.filename.clone(),
                },
            );
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-ready-flags

    inputs.vector_store_id = repo.vector_store_id(scope, chat_id).await?;
    Ok(inputs)
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
