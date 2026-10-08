//! Service container shared by the REST handlers, outbox handlers and workers.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::policy::PolicyGateway;
use crate::infra::llm::LlmGateway;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxEnqueuer;

/// Database provider typed with the domain error.
pub type Db = DBProvider<DomainError>;

/// Shared services.
pub struct AppServices {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<Db>,
    pub raw_db: toolkit_db::Db,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub llm: Arc<LlmGateway>,
    pub metrics: Arc<Metrics>,
    pub hub: Arc<ClientHub>,
    pub upload_permits: Arc<Semaphore>,
    /// Cancelled when the gear stops (background indexing waits, workers).
    pub shutdown: CancellationToken,
}
