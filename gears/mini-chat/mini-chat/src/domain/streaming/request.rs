//! Send request validation (`cpt-cf-mini-chat-algo-message-streaming-request-validation`): the
//! checks that run on the decoded body before the PEP call and before the chat is loaded.

use std::collections::HashSet;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// The decoded body of `POST /v1/chats/{id}/messages:stream` (the JSON extractor already
/// answered 415, 400 `json_syntax_error` and 422 `invalid_json_body`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSendBody {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Option<Vec<Uuid>>,
    /// `web_search.enabled`; `None` for an absent or `null` `web_search` object.
    pub web_search_enabled: Option<bool>,
}

/// The limits of the attachment shape check (`rag.max_documents_per_chat` and
/// `rag.max_images_per_message`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentLimits {
    pub max_documents_per_chat: u32,
    pub max_images_per_message: u32,
}

/// A validated request with the defaults applied.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSend {
    /// As sent: never trimmed, rewritten or truncated.
    pub content: String,
    /// `None` when the client did not supply one (the server generates a UUID v4).
    pub request_id: Option<Uuid>,
    /// Unique, in request order, possibly empty.
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// Validate a decoded body: non-empty content (the emptiness check trims, the content is kept as
/// sent), unique `attachment_ids` within `max_documents_per_chat + max_images_per_message`, and
/// the defaults for the absent fields.
///
/// # Errors
///
/// [`DomainError::EmptyContent`] (checked first) or [`DomainError::InvalidAttachment`] for a
/// duplicate id or too many ids.
pub fn validate_send(
    body: RawSendBody,
    limits: AttachmentLimits,
) -> Result<ValidatedSend, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-empty-content
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-empty
    // only the emptiness check trims (Unicode whitespace); the content itself is kept as sent
    if body.content.trim().is_empty() {
        return Err(DomainError::EmptyContent);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-empty
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-empty-content
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-defaults
    // an absent or `null` `attachment_ids` is an empty list, `web_search` is disabled and a
    // missing `request_id` is "not supplied"
    let attachment_ids = body.attachment_ids.unwrap_or_default();
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-defaults
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-attachment-shape
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-attachment
    let cap = u64::from(limits.max_documents_per_chat) + u64::from(limits.max_images_per_message);
    if attachment_ids.len() as u64 > cap {
        return Err(DomainError::InvalidAttachment);
    }
    let mut seen = HashSet::with_capacity(attachment_ids.len());
    if !attachment_ids.iter().all(|id| seen.insert(*id)) {
        return Err(DomainError::InvalidAttachment);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-attachment
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-attachment-shape
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-content-kept
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return
    // the content is never trimmed, rewritten or truncated; these checks never depend on the
    // chat, which is loaded only after them
    Ok(ValidatedSend {
        content: body.content,
        request_id: body.request_id,
        attachment_ids,
        web_search_enabled: body.web_search_enabled.unwrap_or(false),
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-content-kept
}
