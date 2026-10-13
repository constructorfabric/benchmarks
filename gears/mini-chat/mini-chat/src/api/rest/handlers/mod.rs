//! REST handlers. Each handler extracts the gateway-injected
//! [`SecurityContext`](toolkit_security::SecurityContext) and the
//! [`RestState`](super::RestState), calls one domain service and returns its
//! DTO; domain errors reach the wire as canonical `Problem` responses through
//! `From<DomainError> for CanonicalError` (ADR-0004).
//!
//! Request extraction uses the platform extractors, whose rejections are
//! canonical `Problem`s too: `Path` (a non-UUID id is 400
//! `invalid_path_params`), `Json` (malformed JSON 400 `json_syntax_error`, a
//! schema mismatch 422 `invalid_json_body`, a missing JSON content type 415
//! `missing_json_content_type`) and `OData` (400 on the `OData` query
//! resource type).
//!
//! The attachment upload reads the raw body as a multipart stream (no
//! extractor body limit) and maps its own parse failures to canonical
//! `invalid_argument` problems (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`,
//! `MISSING_FILE`, `MISSING_CONTENT_TYPE`).
//!
//! - [`models`] — Models API;
//! - [`chats`] — chat CRUD;
//! - [`messages`] — message history, reactions and turn status;
//! - [`stream`] — `messages:stream` (SSE);
//! - [`turns`] — retry (SSE), edit (SSE) and delete of the latest turn;
//! - [`quota`] — quota status;
//! - [`attachments`] — attachment upload (multipart), read and delete.

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod models;
pub mod quota;
pub mod stream;
pub mod turns;

pub use attachments::{delete_attachment, get_attachment, upload_attachment};
pub use chats::{create_chat, delete_chat, get_chat, list_chats, update_chat};
pub use messages::{delete_reaction, get_turn, list_messages, put_reaction};
pub use models::{get_model, list_models};
pub use quota::get_quota_status;
pub use stream::stream_message;
pub use turns::{delete_turn, edit_turn, retry_turn};
