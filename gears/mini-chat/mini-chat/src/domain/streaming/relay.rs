//! The events the provider task hands to the SSE relay (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the provider-driven part of the stream
//! contract. `stream_started` and `ping` are produced by the relay itself, the terminal event is
//! produced by the provider task only after the finalization result is known (terminal emission
//! guard).

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::replay::{Delta, Done};

/// The phase of a `tool` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Start,
    Done,
}

/// `tool`: one lifecycle event of a provider tool call.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolEvent {
    pub phase: ToolPhase,
    /// `file_search`, `web_search`, `code_interpreter`, `function_call`, ...
    pub name: String,
    /// Non-sensitive, tenant-safe details (`{}` when there are none).
    pub details: serde_json::Value,
}

/// The source of a citation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    File,
    Web,
}

/// A character range `{start, end}` of the answer text.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// One item of the `citations` event. There is no `score` and no provider identifier.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationItem {
    pub source: CitationSource,
    pub title: String,
    /// Web citations only.
    pub url: Option<String>,
    /// File citations only: the internal attachment id.
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<TextSpan>,
}

/// The payload of the `citations` event.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationsPayload {
    pub items: Vec<CitationItem>,
}

/// The terminal `error` event.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEvent {
    pub code: StreamErrorCode,
    /// Sanitized: no provider identifier, URL or credential.
    pub message: String,
}

/// What the provider task sends to the relay, in order: `delta` and `tool` events as they
/// arrive, at most one `citations`, then the terminal `done` or `error` (or nothing: a lost CAS
/// and a cancelled turn end the channel without a terminal event).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    Delta(Delta),
    Tool(ToolEvent),
    Citations(CitationsPayload),
    Done(Done),
    Error(ErrorEvent),
}
