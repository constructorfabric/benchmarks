//! Send request validation
//! (`cpt-cf-mini-chat-algo-streaming-turns-validate-request`): the pure body
//! checks that run before authorization.

use std::collections::HashSet;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// The decoded `StreamMessageRequest` as the domain receives it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendRequest {
    /// Message text, sent and stored as given.
    pub content: String,
    /// Client idempotency key; `None` generates a UUID v4.
    pub request_id: Option<Uuid>,
    /// Attachment ids; empty equals absent.
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`; `None` (absent or `null`) means `false`.
    pub web_search_enabled: Option<bool>,
}

/// The limits the attachment id checks use.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationLimits {
    /// `rag.max_documents_per_chat`.
    pub max_documents_per_chat: u32,
    /// `rag.max_images_per_message`.
    pub max_images_per_message: u32,
}

/// The validated request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRequest {
    /// Content, unchanged (not trimmed).
    pub content: String,
    /// The given request id or a fresh UUID v4.
    pub request_id: Uuid,
    /// Distinct attachment ids in request order.
    pub attachment_ids: Vec<Uuid>,
    /// Web search flag.
    pub web_search_enabled: bool,
}

/// Validates the body: `EMPTY_CONTENT` first (whitespace-only is empty), then
/// duplicate `attachment_ids` or more than `max_documents_per_chat +
/// max_images_per_message` of them (`invalid_attachment`).
///
/// # Errors
///
/// [`DomainError::EmptyContent`] and [`DomainError::InvalidAttachmentIds`].
#[allow(clippy::trivially_copy_pass_by_ref)] // the limits are a borrowed view of the settings
pub fn validate_request(
    request: &SendRequest,
    limits: &ValidationLimits,
) -> Result<NormalizedRequest, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-if-empty-content
    if request.content.trim().is_empty() {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-empty-content
        return Err(DomainError::EmptyContent);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-empty-content
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-if-empty-content
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-if-ids-invalid
    let max_ids = usize::try_from(
        u64::from(limits.max_documents_per_chat) + u64::from(limits.max_images_per_message),
    )
    .unwrap_or(usize::MAX);
    let mut seen = HashSet::new();
    let distinct = request.attachment_ids.iter().all(|id| seen.insert(*id));
    if !distinct || request.attachment_ids.len() > max_ids {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-ids-invalid
        return Err(DomainError::InvalidAttachmentIds);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-ids-invalid
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-if-ids-invalid
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-normalized
    Ok(NormalizedRequest {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-keep-content
        content: request.content.clone(),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-keep-content
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-resolve-request-id
        request_id: request.request_id.unwrap_or_else(Uuid::new_v4),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-resolve-request-id
        attachment_ids: request.attachment_ids.clone(),
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-web-search-flag
        web_search_enabled: request.web_search_enabled.unwrap_or(false),
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-web-search-flag
    })
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-validate-request:p1:inst-return-normalized
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
