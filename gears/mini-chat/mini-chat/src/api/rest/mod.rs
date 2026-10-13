//! REST API of the gear (DESIGN section 3.3 "API Contracts").
//!
//! - [`routes`] — operation registration through the platform
//!   `OperationBuilder` ([`register_routes`]);
//! - [`handlers`] — request handlers over the domain services;
//! - [`dto`] — wire DTOs of `docs/api/api.json`;
//! - [`sse`] — the SSE relay of a streaming turn.
//!
//! Errors reach the wire as canonical `Problem` responses built from
//! `crate::domain::error::DomainError` (ADR-0004).
//!
//! The gear wires the surface in `register_rest`:
//!
//! ```ignore
//! let state = RestState {
//!     models, chats, messages, stream, turns, quota, attachments, sse_ping_interval,
//! };
//! let router = api::rest::register_routes(router, openapi, state, &config.url_prefix);
//! ```

use std::sync::Arc;
use std::time::Duration;

use crate::domain::attachment_service::AttachmentService;
use crate::domain::chat_service::ChatService;
use crate::domain::message_service::MessageService;
use crate::domain::models_service::ModelsService;
use crate::domain::quota_service::QuotaService;
use crate::domain::stream::StreamService;
use crate::domain::turn_service::TurnService;

pub mod dto;
pub mod handlers;
pub mod routes;
pub mod sse;

pub use routes::register_routes;

/// Domain services shared by the handlers; attached once to the router as an
/// `Extension` and cloned per request.
#[derive(Debug, Clone)]
pub struct RestState {
    /// Models API service.
    pub models: Arc<ModelsService>,
    /// Chat CRUD service.
    pub chats: Arc<ChatService>,
    /// Message history, reaction and turn status service.
    pub messages: Arc<MessageService>,
    /// Streaming turn service.
    pub stream: Arc<StreamService>,
    /// Turn mutation service (retry, edit, delete).
    pub turns: Arc<TurnService>,
    /// Quota service (quota status).
    pub quota: Arc<QuotaService>,
    /// Attachment upload, read and delete service.
    pub attachments: Arc<AttachmentService>,
    /// `streaming.sse_ping_interval_seconds`.
    pub sse_ping_interval: Duration,
}
