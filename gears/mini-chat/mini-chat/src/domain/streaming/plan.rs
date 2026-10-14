//! The hand-over to the provider task (slice S2): the assembled [`ProviderCallPlan`] and the
//! outcome of the send setup.

use async_trait::async_trait;
use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::provider_registry::EffectiveProvider;
use crate::domain::quota::{PreflightDecision, ReserveReceipt};
use crate::domain::streaming::context::ContextAssembly;
use crate::domain::streaming::relay::RelayEvent;
use crate::domain::streaming::replay::StreamEvent;
use crate::domain::streaming::store::ReservedTurn;
use crate::domain::streaming::tools::ToolSet;

/// Everything the provider task needs once the reserve transaction committed.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCallPlan {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    /// Pre-allocated UUID v4; no assistant row exists until finalization.
    pub assistant_message_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// The catalog entry of the effective model (`provider_model_id`, `api_params`, ...).
    pub model: ModelCatalogEntry,
    pub decision: PreflightDecision,
    pub receipt: ReserveReceipt,
    pub context: ContextAssembly,
    pub tools: ToolSet,
    /// The resolved provider of the effective model for the caller's tenant.
    pub provider: EffectiveProvider,
    /// The requested flag (stored on the turn whether or not the tool is in the set).
    pub web_search_enabled: bool,
    /// `chat_turns.started_at`, the preflight instant.
    pub started_at: OffsetDateTime,
}

/// The provider task of slice S2: receives the plan after the reserve transaction committed and
/// the cancellation check passed. The send setup never calls a provider itself.
#[async_trait]
pub trait ProviderTaskPort: Send + Sync {
    /// Start the provider task for the plan; `cancel` is the turn's cancellation token (cancelled
    /// by a dropped response or a dropped SSE relay).
    ///
    /// Returns the receiver of the bounded relay channel, which carries the provider driven
    /// events and the terminal event of the stream.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the task cannot be started.
    async fn start(
        &self,
        plan: ProviderCallPlan,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<RelayEvent>, DomainError>;
}

/// A turn whose provider task runs: the plan and the receiver of its relay channel.
#[domain_model]
#[derive(Debug)]
pub struct StartedTurn {
    pub plan: ProviderCallPlan,
    pub events: mpsc::Receiver<RelayEvent>,
}

/// The outcome of the send setup.
#[domain_model]
#[derive(Debug)]
pub enum SendOutcome {
    /// A completed turn of the supplied `request_id`: the replay events; nothing was written,
    /// reserved, enqueued or called.
    Replay(Vec<StreamEvent>),
    /// The reserve transaction committed and the real provider task runs: the SSE relay
    /// consumes its channel.
    Streaming(Box<StartedTurn>),
    /// The cancellation token was already cancelled after the reserve commit: no provider
    /// request was sent and the committed turn was finalized `cancelled` (estimated settlement,
    /// `aborted` usage event, no assistant message) before the setup returned.
    CancelledBeforeProvider(Box<ReservedTurn>),
}
