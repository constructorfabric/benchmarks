//! Stream service: send, replay, retry/edit, the provider task and finalization.

pub mod citations;
pub mod events;
pub mod finalize;
pub mod run;
pub mod setup;

use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use self::citations::FileMap;
use self::events::{StreamEvent, StreamStarted};
use crate::domain::knowledge::KnowledgeParams;
use crate::domain::quota::PreflightDecision;
use crate::infra::llm::ResolvedProvider;
use crate::infra::llm::responses::ChatRequest;

/// Thread-summary trigger inputs computed during context assembly.
#[allow(clippy::struct_excessive_bools)] // independent facts of the context assembly
#[derive(Debug, Clone, Copy)]
pub struct SummaryTrigger {
    pub evaluate: bool,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub truncated: bool,
    pub has_summary: bool,
}

/// In-memory finalization context of a running turn.
#[derive(Debug, Clone)]
pub struct TurnRuntime {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// Pre-allocated assistant message id.
    pub message_id: Uuid,
    pub selected_model: String,
    pub decision: PreflightDecision,
    pub provider: ResolvedProvider,
    pub request: ChatRequest,
    pub files: FileMap,
    /// Knowledge search parameters when `search_knowledge` is offered.
    pub knowledge: Option<KnowledgeParams>,
    pub summary_trigger: SummaryTrigger,
    pub started: Instant,
    pub started_at: time::OffsetDateTime,
}

/// Result of a stream setup.
pub enum StreamStart {
    /// Buffered replay of a completed turn.
    Replay(Vec<StreamEvent>),
    /// Live generation.
    Live {
        started: StreamStarted,
        rx: mpsc::Receiver<StreamEvent>,
        cancel: CancellationToken,
    },
}
