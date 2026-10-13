//! The send-message pipeline: preflight, reserve, provider relay and finalization.

mod facts;
mod finalize;
mod model;
mod ports;
mod replay;
mod run;
mod service;

pub use facts::{TerminalFacts, TurnFacts, build_events, dedupe_key};
pub use finalize::{FinalizeError, Finalizer, Terminal};
pub use model::{
    AssistantMessage, AttachmentRef, CitationItem, CitationSource, DeltaKind, DonePayload,
    EventStream, FinalizeCommand, FinalizeOutcome, MutationPreflight, MutationReserveCommand,
    NewTurnIds, PreparedMutation, ReserveCommand, SendInput, StreamEvent, SummaryRequest,
    SummaryView, ToolCounts, ToolPhase, TurnContext, secondary_image_ids,
};
pub use ports::{ConversationRepository, StoredAssistantMessage, TurnRepository};
pub use replay::ReplayService;
pub use run::{RunDeps, RunSettings, TurnRun};
pub use service::{StreamConfig, StreamService, StreamServiceParts};
