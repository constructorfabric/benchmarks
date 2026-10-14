//! Edit request validation (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation`):
//! the emptiness check of the edit `content`. The body checks of the platform extractor (415,
//! `json_syntax_error`, 422) and the path checks run in the handler.

use toolkit_macros::domain_model;

use crate::domain::error::DomainError;

/// The validated body of `PATCH /v1/chats/{id}/turns/{request_id}`: the `content` exactly as
/// sent (never trimmed, rewritten or truncated).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEdit {
    pub content: String,
}

/// Validate the `content` of an edit: empty or whitespace-only is `EmptyContent`; otherwise the
/// content is kept as sent.
///
/// # Errors
///
/// `EmptyContent` for an empty or whitespace-only content.
pub fn validate_edit_content(content: String) -> Result<ValidatedEdit, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-empty
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-empty
    if content.trim().is_empty() {
        return Err(DomainError::EmptyContent);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-empty
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-empty
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-kept
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return
    Ok(ValidatedEdit { content })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-kept
}
