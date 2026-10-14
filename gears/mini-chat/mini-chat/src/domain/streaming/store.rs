//! The persistence port of the send setup, implemented by the infrastructure layer over the
//! database provider.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::chats::ChatStore;
use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::quota::{PreflightDecision, QuotaService, ReserveReceipt};
use crate::domain::streaming::context::{ContextMessage, ThreadSummaryRow};
use crate::domain::turns::TurnRow;

/// The snapshot boundary `(max_included_created_at, max_included_id)`: the latest non-deleted
/// message of the chat in the order `(created_at, id)`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBoundary {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

/// An attachment referenced by the request, as the preflight classifies it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFact {
    pub id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// The internal provider file id (never exposed), `None` until the upload completed.
    pub provider_file_id: Option<String>,
    /// The file id of the secondary Anthropic copy of an image (internal only), `None` when
    /// there is none.
    pub secondary_file_id: Option<String>,
    /// `not_attempted`, `pending`, `uploaded` or `failed`: only `uploaded` makes the secondary
    /// file id usable.
    pub secondary_status: String,
}

/// The `secondary_status` of a secondary copy that finished uploading.
pub const SECONDARY_UPLOADED: &str = "uploaded";

impl AttachmentFact {
    /// The secondary Anthropic file id when the secondary copy is uploaded, `None` otherwise.
    #[must_use]
    pub fn usable_secondary_file_id(&self) -> Option<&str> {
        if self.secondary_status == SECONDARY_UPLOADED {
            self.secondary_file_id
                .as_deref()
                .filter(|id| !id.is_empty())
        } else {
            None
        }
    }
}

/// The reads of the send preflight before the quota call.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReads {
    /// `None` for the first message of an empty chat.
    pub boundary: Option<SnapshotBoundary>,
    /// The referenced attachments that exist in the scoped chat (not soft-deleted), in
    /// `attachment_ids` order; an id that does not exist there is absent.
    pub attachments: Vec<AttachmentFact>,
    /// Ready, non-deleted document attachments of the chat.
    pub ready_documents: u32,
    /// Ready, non-deleted attachments of the chat with `for_code_interpreter`.
    pub ready_code_interpreter: u32,
}

/// The reads of the context assembly.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReads {
    pub summary: Option<ThreadSummaryRow>,
    /// At most `limit` messages, chronological: `request_id IS NOT NULL`, not deleted, not
    /// compressed, `(created_at, id)` at most the boundary and, only with a summary, greater
    /// than its frontier.
    pub recent: Vec<ContextMessage>,
}

/// The input of the reserve transaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ReserveCommand {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    pub content: String,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
    pub decision: PreflightDecision,
    /// The application clock reading of the setup (`QuotaService::clock`): the user message gets
    /// `created_at = max(now, latest message created_at + 1 µs)`.
    pub now: OffsetDateTime,
}

/// A committed reserve transaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ReservedTurn {
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    /// The `created_at` written on the user message.
    pub user_message_created_at: OffsetDateTime,
    /// `chat_turns.started_at`: the preflight instant of the decision.
    pub started_at: OffsetDateTime,
    pub receipt: ReserveReceipt,
}

/// The persistence of the send setup. The chat load comes from [`ChatStore`].
#[async_trait]
pub trait StreamStore: ChatStore {
    /// The `(chat_id, request_id)` turn of the tenant, soft-deleted rows included.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn find_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError>;

    /// Whether the chat has a `running` turn that is not soft-deleted, whatever its `request_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn has_running_turn(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError>;

    /// The non-deleted message `message_id` of the chat (the assistant message of a replay).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn load_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError>;

    /// The snapshot boundary, the referenced attachments and the chat facts of the quota estimate.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn read_preflight(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<PreflightReads, DomainError>;

    /// The thread summary and the recent messages (at most `limit`) within the boundary.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn read_context(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: Option<&SnapshotBoundary>,
        limit: u32,
    ) -> Result<ContextReads, DomainError>;

    /// The reserve transaction: `quota.write_reserve` (`ReserveTurn::New`), the user message
    /// (`created_at` after the latest message of the chat), the `chats.updated_at` bump, the
    /// attachment validation and links, and the `running` turn; one transaction, committed or
    /// rolled back as a whole. The reserve metrics are recorded by the caller after the commit.
    ///
    /// # Errors
    ///
    /// `QuotaExceeded { Tokens }` for the re-check, `InvalidAttachment`, `Conflict` for a key or
    /// running-index race, `Internal` for any other failure; nothing is left behind.
    async fn reserve(
        &self,
        quota: &QuotaService,
        command: &ReserveCommand,
    ) -> Result<ReservedTurn, DomainError>;
}
