//! Domain services (one `Core` shared by handlers, outbox handlers and workers).

pub mod attachments;
pub mod chats;
pub mod cleanup;
pub mod finalize;
pub mod messages;
pub mod models;
pub mod quota;
pub mod reactions;
pub mod stream;
pub mod summary;
pub mod turns;

use std::sync::{Arc, RwLock};

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::infra::llm::ProviderTransport;
use crate::infra::llm::provider::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;
use crate::infra::outbox::OutboxDispatch;
use crate::infra::plugin_gateway::{AuditGateway, PolicyGateway};

pub type Db = DBProvider<DomainError>;

/// Security context used for background provider calls (S2S identity obtained at start).
#[derive(Default)]
pub struct SystemContext {
    inner: RwLock<Option<SecurityContext>>,
}

impl SystemContext {
    pub fn set(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.inner.write() {
            *g = Some(ctx);
        }
    }

    /// The S2S context, or a context for the given tenant with the platform default subject.
    #[must_use]
    pub fn get_or_tenant(&self, tenant_id: uuid::Uuid) -> SecurityContext {
        if let Ok(g) = self.inner.read()
            && let Some(c) = g.as_ref()
        {
            return c.clone();
        }
        SecurityContext::builder()
            .subject_id(SYSTEM_SUBJECT_ID)
            .subject_tenant_id(tenant_id)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous())
    }
}

/// Platform default subject id (used for system identity in provider metadata).
pub const SYSTEM_SUBJECT_ID: uuid::Uuid =
    uuid::Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);

/// Shared state of all services.
pub struct Core {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<Db>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub transport: Arc<dyn ProviderTransport>,
    pub storage: Arc<StorageClient>,
    pub providers: Arc<ProviderRegistry>,
    pub outbox: Arc<OutboxDispatch>,
    pub system_ctx: Arc<SystemContext>,
    pub upload_slots: Arc<Semaphore>,
    pub upload_timings: attachments::UploadTimings,
    pub metrics: crate::infra::metrics::Metrics,
    pub shutdown: CancellationToken,
}

/// Construction parameters of [`Core`].
pub struct CoreDeps {
    pub cfg: MiniChatConfig,
    pub db: Arc<Db>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub transport: Arc<dyn ProviderTransport>,
}

impl Core {
    #[must_use]
    pub fn new(deps: CoreDeps) -> Arc<Self> {
        let mut cfg = deps.cfg;
        cfg.fill_aliases();
        let providers = Arc::new(ProviderRegistry::new(cfg.providers.clone()));
        let outbox = Arc::new(OutboxDispatch::new(cfg.outbox.clone()));
        let slots = Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads)));
        let metrics = crate::infra::metrics::Metrics::global(&cfg.metrics.prefix);
        let storage = Arc::new(StorageClient::new(Arc::clone(&deps.transport)));
        Arc::new(Self {
            cfg: Arc::new(cfg),
            db: deps.db,
            enforcer: deps.enforcer,
            policy: deps.policy,
            audit: deps.audit,
            transport: deps.transport,
            storage,
            providers,
            outbox,
            system_ctx: Arc::new(SystemContext::default()),
            upload_slots: slots,
            upload_timings: attachments::UploadTimings::default(),
            metrics,
            shutdown: CancellationToken::new(),
        })
    }
}

#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Logs the failure of a best-effort operation whose outcome is intentionally ignored.
pub(crate) fn log_best_effort<T, E: std::fmt::Display>(res: Result<T, E>, what: &str) {
    if let Err(e) = res {
        tracing::debug!(error = %e, "mini-chat: best-effort {what} failed");
    }
}

/// Provider `user` field: tenant + user UUIDs in simple form (64 chars).
#[must_use]
pub fn provider_user_field(tenant_id: uuid::Uuid, user_id: uuid::Uuid) -> String {
    format!("{}{}", tenant_id.simple(), user_id.simple())
}
