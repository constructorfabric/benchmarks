//! Shared service container passed to handlers, domain services, workers and outbox handlers.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuditPort, AuthzPort, PolicyPort};
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::llm::transport::ProviderTransport;
use crate::infra::outbox::OutboxPort;

pub type Db = DBProvider<DomainError>;

/// Everything the gear's services need.
pub struct AppServices {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<Db>,
    pub authz: Arc<dyn AuthzPort>,
    pub policy: Arc<dyn PolicyPort>,
    pub audit: Arc<dyn AuditPort>,
    pub transport: Arc<dyn ProviderTransport>,
    pub providers: ProviderResolver,
    pub outbox: Arc<OutboxPort>,
    /// Per-process upload concurrency limit (`rag.max_concurrent_uploads`).
    pub upload_permits: Arc<Semaphore>,
    /// Cancelled when the gear stops (background tasks).
    pub shutdown: CancellationToken,
    /// OAGW provisioning state of provider aliases.
    pub provisioning: Arc<crate::infra::llm::gate::ProvisioningGate>,
}

impl AppServices {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        cfg: Arc<MiniChatConfig>,
        db: Arc<Db>,
        authz: Arc<dyn AuthzPort>,
        policy: Arc<dyn PolicyPort>,
        audit: Arc<dyn AuditPort>,
        transport: Arc<dyn ProviderTransport>,
    ) -> Self {
        let providers = ProviderResolver::new(Arc::clone(&cfg));
        let outbox = Arc::new(OutboxPort::new(cfg.outbox.clone()));
        let upload_permits = Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads)));
        Self {
            cfg,
            db,
            authz,
            policy,
            audit,
            transport,
            providers,
            outbox,
            upload_permits,
            shutdown: CancellationToken::new(),
            provisioning: Arc::new(crate::infra::llm::gate::ProvisioningGate::default()),
        }
    }
}
