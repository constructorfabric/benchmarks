//! The durable progress and counter updates of a running turn (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`, algorithm `tool-accounting`): the repository
//! port the provider task writes through. Every update is guarded by `state = 'running'`, so
//! the orphan watchdog can recover the counts and a finalized turn is never touched.

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::streaming::tools::ToolKind;

/// Progress and counter writes of a `running` turn.
#[async_trait]
pub trait TurnProgress: Send + Sync {
    /// Increment `web_search_completed_count`, `code_interpreter_completed_count` or
    /// `file_search_completed_count` of the turn by one (`search_knowledge` counts as
    /// `file_search`); guarded by `state = 'running'`: a turn in another state is unchanged.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn record_tool_done(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        tool: ToolKind,
    ) -> Result<(), DomainError>;

    /// Set `last_progress_at` (and `updated_at`) of the turn to `at`; guarded by
    /// `state = 'running'`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn touch_progress(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<(), DomainError>;
}
