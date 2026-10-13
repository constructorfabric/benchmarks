//! Stream flow types: the request, the SSE events and the persistence commands.

use std::collections::HashMap;
use std::time::Duration;

use mini_chat_sdk::{TokenUsage, TurnAuditEvent, UsageEvent};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::billing::Settlement;
use crate::domain::quota::{PreflightDecision, QuotaPeriod, QuotaWarning, ReservePlan};
use crate::domain::summary::SummaryTrigger;
use crate::domain::turn::TurnState;

/// The body of `POST messages:stream`.
#[domain_model]
#[derive(Debug, Clone)]
pub struct SendInput {
    /// The user message (unchecked).
    pub content: String,
    /// The client-supplied idempotency key.
    pub request_id: Option<Uuid>,
    /// Attachments referenced by the message.
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`.
    pub web_search: bool,
}

/// The kind of a `delta` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Answer text.
    Text,
    /// Model reasoning (vLLM `<think>` blocks).
    Reasoning,
}

/// The phase of a `tool` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// The call started.
    Start,
    /// The call finished.
    Done,
}

/// The source of a citation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    /// An uploaded document.
    File,
    /// A web page.
    Web,
}

/// One item of a `citations` event.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationItem {
    /// File or web.
    pub source: CitationSource,
    /// The filename or the page title.
    pub title: String,
    /// The page URL (web only).
    pub url: Option<String>,
    /// The attachment (file only).
    pub attachment_id: Option<Uuid>,
    /// The excerpt; empty for files.
    pub snippet: String,
    /// The annotated range, when the provider gave both ends.
    pub span: Option<(u64, u64)>,
}

/// The payload of the terminal `done` event.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct DonePayload {
    /// Provider-reported input tokens.
    pub input_tokens: i64,
    /// Provider-reported output tokens.
    pub output_tokens: i64,
    /// The model actually used.
    pub effective_model: String,
    /// The chat's model.
    pub selected_model: String,
    /// `allow` or `downgrade`.
    pub quota_decision: &'static str,
    /// The selected model, when downgraded.
    pub downgrade_from: Option<String>,
    /// Why, when known.
    pub downgrade_reason: Option<&'static str>,
    /// Quota status after the settlement; absent on replay.
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// An event of the stream relay.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The first event of every stream.
    Started {
        /// The turn's request id.
        request_id: Uuid,
        /// The pre-allocated (new turn) or persisted (replay) assistant message id.
        message_id: Uuid,
        /// `false` on replay.
        is_new_turn: bool,
        /// The token estimate of the thread summary that is part of the context.
        thread_summary_tokens: Option<i64>,
    },
    /// Incremental output.
    Delta {
        /// Text or reasoning.
        kind: DeltaKind,
        /// The fragment.
        content: String,
    },
    /// Tool activity.
    Tool {
        /// Start or done.
        phase: ToolPhase,
        /// The tool name.
        name: String,
        /// Tool-specific details.
        details: serde_json::Value,
    },
    /// Source references of the answer.
    Citations(Vec<CitationItem>),
    /// The terminal success event.
    Done(DonePayload),
    /// The terminal failure event.
    Error {
        /// The streaming error code.
        code: String,
        /// The sanitized message.
        message: String,
    },
}

impl StreamEvent {
    /// `done` or `error`.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// `delta`, `tool`, `citations` or a terminal event: nothing may be pinged after it.
    #[must_use]
    pub fn is_content(&self) -> bool {
        !matches!(self, Self::Started { .. })
    }
}

/// A live relay: the bounded event channel and the ping interval.
pub struct EventStream {
    /// The events, ending after the terminal one (or without it when the provider task died).
    pub events: mpsc::Receiver<StreamEvent>,
    /// Seconds between pings before the first content event.
    pub ping_interval: Duration,
}

/// The attachments referenced by a message, as the stream reads them.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    /// Attachment id.
    pub id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// The filename.
    pub filename: String,
    /// The provider file id (images of the current turn are sent by it).
    pub provider_file_id: Option<String>,
    /// The id of the secondary copy in the Anthropic Files API (images of Anthropic chats); an
    /// `anthropic_messages` turn sends the image by it.
    pub secondary_file_id: Option<String>,
}

/// The secondary (Anthropic Files API) copy ids of the images among `attachments`, by the primary
/// provider file id.
#[must_use]
pub fn secondary_image_ids(attachments: &[AttachmentRef]) -> HashMap<String, String> {
    attachments
        .iter()
        .filter(|attachment| attachment.kind == "image")
        .filter_map(|attachment| {
            Some((
                attachment.provider_file_id.clone()?,
                attachment.secondary_file_id.clone()?,
            ))
        })
        .collect()
}

/// The thread summary of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryView {
    /// The summary text.
    pub text: String,
    /// The stored token estimate.
    pub token_estimate: i64,
}

/// Everything the preflight reads about the conversation.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct TurnContext {
    /// `input_tokens + output_tokens` of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    /// The thread summary, when one exists.
    pub summary: Option<SummaryView>,
    /// Recent messages, oldest first, within the snapshot boundary and after the summary frontier.
    pub history: Vec<crate::domain::llm::InputMessage>,
    /// The chat has at least one ready, non-deleted attachment with `for_file_search = true`.
    pub ready_documents: bool,
    /// The provider vector store of the chat.
    pub vector_store_id: Option<String>,
    /// Provider ids of the ready code-interpreter files.
    pub code_interpreter_file_ids: Vec<String>,
}

/// The reserve transaction of a new turn.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ReserveCommand {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The turn's request id.
    pub request_id: Uuid,
    /// The new turn.
    pub turn_id: Uuid,
    /// The new user message.
    pub user_message_id: Uuid,
    /// The user message text.
    pub content: String,
    /// Attachments to link to the user message.
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled` of the request.
    pub web_search_enabled: bool,
    /// The effective model.
    pub effective_model: String,
    /// The policy version of the preflight.
    pub policy_version: i64,
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i32,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i32,
    /// The quota rows to book and re-check.
    pub plan: ReservePlan,
    /// The instant of the transaction (user message, turn start).
    pub now: OffsetDateTime,
}

/// The assistant message of a finalization.
#[domain_model]
#[derive(Debug, Clone)]
pub struct AssistantMessage {
    /// The pre-allocated message id.
    pub id: Uuid,
    /// The text.
    pub content: String,
    /// The effective model.
    pub model: String,
    /// Provider usage; `None` stores zeros.
    pub usage: Option<TokenUsage>,
    /// The provider response id (internal).
    pub provider_response_id: Option<String>,
}

/// Completed tool calls of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounts {
    /// Completed `web_search` calls.
    pub web_search: u32,
    /// Completed `code_interpreter` calls.
    pub code_interpreter: u32,
    /// Completed `file_search` calls.
    pub file_search: u32,
}

/// A summary trigger carried to the finalization transaction of a completed turn.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct SummaryRequest {
    /// What the context assembly said.
    pub trigger: SummaryTrigger,
    /// The `system_request_id` of the work item, when one is enqueued.
    pub system_request_id: Uuid,
}

/// The finalization transaction of a turn: CAS, assistant message, settlement and outbox events.
#[domain_model]
#[derive(Debug, Clone)]
pub struct FinalizeCommand {
    /// The turn.
    pub turn_id: Uuid,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The turn's request id (shared by its messages).
    pub request_id: Uuid,
    /// The terminal state to CAS into.
    pub state: TurnState,
    /// Terminal error code of a failed turn.
    pub error_code: Option<String>,
    /// Non-sensitive diagnostic text of a failed turn.
    pub error_detail: Option<String>,
    /// The provider response id (internal).
    pub provider_response_id: Option<String>,
    /// The assistant message to persist.
    pub assistant_message: Option<AssistantMessage>,
    /// Completed tool calls.
    pub tool_counts: ToolCounts,
    /// The quota periods of the turn's reserve (derived from its start).
    pub periods: [QuotaPeriod; 2],
    /// The turn's reserve, released by the settlement.
    pub reserved_credits_micro: i64,
    /// Whether the turn ran on the premium tier; `None` when its model could not be resolved, in
    /// which case the premium bucket is released only where every period row still holds at least
    /// the reserve.
    pub premium: Option<bool>,
    /// Settle the quota rows of the turn's reserve; `false` for a turn that holds no reserve
    /// (orphaned retry/edit turn).
    pub book_quota: bool,
    /// The orphan watchdog's stale-progress cutoff: when set, the CAS additionally requires
    /// `deleted_at IS NULL` and `COALESCE(last_progress_at, started_at) <= cutoff`.
    pub orphan_cutoff: Option<OffsetDateTime>,
    /// The orphan watchdog finalizes without settlement because its read of the turn had NULL
    /// reserve fields: the CAS additionally requires `reserve_tokens IS NULL`, so a reserve filled
    /// after that read is never finalized unsettled (the CAS loses; the next scan settles it).
    pub require_unreserved: bool,
    /// The settlement.
    pub settlement: Settlement,
    /// The instant of the finalization.
    pub now: OffsetDateTime,
    /// The usage event to enqueue.
    pub usage_event: UsageEvent,
    /// The audit event to enqueue.
    pub audit_event: TurnAuditEvent,
    /// The summary trigger of a completed turn, evaluated in the same transaction.
    pub summary: Option<SummaryRequest>,
}

/// The result of a finalization attempt.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// This finalizer won the CAS and committed.
    Committed,
    /// Another finalizer already moved the turn out of `running`; nothing was written.
    Lost,
}

/// What a retry or edit asks the stream service to preflight: the replacement turn as it will be
/// sent.
#[domain_model]
#[derive(Debug, Clone)]
pub struct MutationPreflight {
    /// The user message of the new turn (the original content for a retry, the new one for an
    /// edit).
    pub content: String,
    /// `web_search_enabled` of the replaced turn.
    pub web_search: bool,
    /// Provider file ids of the original message's live images.
    pub image_ids: Vec<String>,
    /// The ids of the secondary (Anthropic Files API) copies of those images, by provider file id.
    pub image_secondary_ids: HashMap<String, String>,
    /// Number of the original message's live images.
    pub image_count: usize,
    /// The request of the turn that is replaced: its messages are not part of the context.
    pub replaced_request: Uuid,
}

/// A retry or edit that passed the preflight and waits for the mutation commit.
#[domain_model]
#[derive(Debug, Clone)]
pub struct PreparedMutation {
    pub(super) input: MutationPreflight,
    pub(super) decision: PreflightDecision,
    pub(super) limits: mini_chat_sdk::UserLimits,
    pub(super) now: OffsetDateTime,
}

impl PreparedMutation {
    /// The instant the preflight ran: the start of the new turn and its quota periods.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        self.now
    }

    /// The content of the new user message.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.input.content
    }
}

/// The identities of the turn a retry or edit creates (server-generated).
#[domain_model]
#[allow(
    clippy::struct_field_names,
    reason = "every field is the identifier of one row"
)]
#[derive(Debug, Clone, Copy)]
pub struct NewTurnIds {
    /// The new turn's `request_id` (UUID v4).
    pub request_id: Uuid,
    /// The new turn.
    pub turn_id: Uuid,
    /// The new user message.
    pub user_message_id: Uuid,
    /// The pre-allocated assistant message.
    pub assistant_message_id: Uuid,
}

impl NewTurnIds {
    /// Fresh identities.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
        }
    }
}

/// Fills the preflight columns of an unstarted retry/edit turn and books its quota reserve.
#[domain_model]
#[derive(Debug, Clone)]
pub struct MutationReserveCommand {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The new turn.
    pub turn_id: Uuid,
    /// The effective model.
    pub effective_model: String,
    /// The policy version of the preflight.
    pub policy_version: i64,
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i32,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i32,
    /// The quota rows to book and re-check.
    pub plan: ReservePlan,
    /// The instant of the reserve (the turn's start).
    pub now: OffsetDateTime,
}
