//! Message streaming, slice S1-setup-path (feature
//! `cpt-cf-mini-chat-feature-message-streaming`): everything the send pipeline does before the
//! provider call: request validation, the turn admission checks, the replay stream, the send
//! preflight (image guards, context budget, context assembly, tool assembly), the reserve
//! transaction and the hand-over of the assembled [`ProviderCallPlan`] to the provider task.
//!
//! Slice S2-provider-path adds the provider call and request building, the adapters, the event
//! translation, tool accounting, citations, the stream finalization contract, the terminal
//! emission matrix, the turn audit event, the summary trigger and the orphan watchdog; they
//! consume the plan.

pub mod admission;
pub mod audit;
pub mod budget;
pub mod citations;
pub mod context;
pub mod finalizer;
pub mod image_guards;
pub mod outbox;
pub mod plan;
pub mod progress;
pub mod provider_errors;
pub mod provider_events;
pub mod provider_request;
pub mod relay;
pub mod replay;
pub mod request;
pub mod service;
pub mod store;
pub mod summary;
pub mod terminal;
pub mod tool_accounting;
pub mod tools;

pub use admission::{AdmissionDecision, decide_admission};
pub use budget::{ContextBudget, compute_budget};
pub use context::{
    ContextAssembly, ContextItem, ContextMessage, ContextPlan, ContextRole, CurrentMessage,
    ImageInput, THREAD_SUMMARY_PREAMBLE, ThreadSummaryRow, UserItem, assemble_context,
};
pub use finalizer::{
    CommittedTurn, StreamFinalization, StreamOutcome, StreamTerminal, TurnFinalization,
    TurnFinalizer, TurnOutcome, TurnTarget,
};
pub use image_guards::check_image_guards;
pub use outbox::TurnOutbox;
pub use plan::{ProviderCallPlan, ProviderTaskPort, SendOutcome, StartedTurn};
pub use progress::TurnProgress;
pub use relay::{
    CitationItem, CitationSource, CitationsPayload, ErrorEvent, RelayEvent, TextSpan, ToolEvent,
    ToolPhase,
};
pub use replay::{Delta, DeltaKind, Done, StreamEvent, StreamStarted, build_replay};
pub use request::{AttachmentLimits, RawSendBody, ValidatedSend, validate_send};
pub use service::{
    MessageStreamService, PreparedTurn, ReplacementAssembly, ReplacementRequest, StreamSettings,
};
pub use store::{
    AttachmentFact, ContextReads, PreflightReads, ReserveCommand, ReservedTurn, SnapshotBoundary,
    StreamStore,
};
pub use tools::{
    ToolContribution, ToolDefinition, ToolKind, ToolPart, ToolSet, ToolTurnFacts, assemble_tools,
};

#[cfg(test)]
#[path = "streaming/admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "streaming/audit_tests.rs"]
mod audit_tests;

#[cfg(test)]
#[path = "streaming/citations_tests.rs"]
mod citations_tests;

#[cfg(test)]
#[path = "streaming/provider_errors_tests.rs"]
mod provider_errors_tests;

#[cfg(test)]
#[path = "streaming/provider_events_tests.rs"]
mod provider_events_tests;

#[cfg(test)]
#[path = "streaming/provider_request_tests.rs"]
mod provider_request_tests;

#[cfg(test)]
#[path = "streaming/summary_tests.rs"]
mod summary_tests;

#[cfg(test)]
#[path = "streaming/terminal_tests.rs"]
mod terminal_tests;

#[cfg(test)]
#[path = "streaming/tool_accounting_tests.rs"]
mod tool_accounting_tests;

#[cfg(test)]
#[path = "streaming/budget_tests.rs"]
mod budget_tests;

#[cfg(test)]
#[path = "streaming/context_tests.rs"]
mod context_tests;

#[cfg(test)]
#[path = "streaming/image_guards_tests.rs"]
mod image_guards_tests;

#[cfg(test)]
#[path = "streaming/replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
#[path = "streaming/request_tests.rs"]
mod request_tests;

#[cfg(test)]
#[path = "streaming/tools_tests.rs"]
mod tools_tests;

#[cfg(test)]
#[path = "streaming/send_setup_tests.rs"]
mod send_setup_tests;

#[cfg(test)]
#[path = "streaming/tool_bound_tests.rs"]
mod tool_bound_tests;

#[cfg(test)]
#[path = "streaming/attachment_tools_tests.rs"]
mod attachment_tools_tests;
