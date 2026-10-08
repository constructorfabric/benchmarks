//! Shared service container used by handlers, workers and outbox handlers.

use std::sync::Arc;

use authz_resolver_sdk::pep::PolicyEnforcer;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_security::pep_properties::{OWNER_ID, OWNER_TENANT_ID};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter};
use uuid::Uuid;

use super::error::DomainError;
use crate::config::MiniChatConfig;
use crate::infra::gateways::{AuditGateway, PolicyGateway};
use crate::infra::llm::storage::StorageClient;
use crate::infra::llm::{ProviderResolver, ProviderTransport};
use crate::infra::outbox::OutboxEnqueuer;

/// All dependencies of the gear's services.
pub struct App {
    pub cfg: MiniChatConfig,
    pub db: DBProvider<DomainError>,
    pub enforcer: PolicyEnforcer,
    pub policy: PolicyGateway,
    pub audit: AuditGateway,
    pub transport: Arc<dyn ProviderTransport>,
    pub storage: StorageClient,
    pub resolver: ProviderResolver,
    pub outbox: OutboxEnqueuer,
    pub upload_slots: Arc<tokio::sync::Semaphore>,
    pub shutdown: CancellationToken,
    /// Upload indexing deadline (25 s; tests shorten it).
    pub indexing_deadline: std::time::Duration,
    /// Background indexing limit (10 min; tests shorten it).
    pub background_indexing_limit: std::time::Duration,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        cfg: MiniChatConfig,
        db: DBProvider<DomainError>,
        enforcer: PolicyEnforcer,
        policy: PolicyGateway,
        audit: AuditGateway,
        transport: Arc<dyn ProviderTransport>,
    ) -> Self {
        let resolver = ProviderResolver::new(cfg.providers.clone());
        let outbox = OutboxEnqueuer::new(cfg.outbox.clone());
        let slots = Arc::new(tokio::sync::Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads)));
        Self {
            storage: StorageClient::new(Arc::clone(&transport)),
            cfg,
            db,
            enforcer,
            policy,
            audit,
            transport,
            resolver,
            outbox,
            upload_slots: slots,
            shutdown: CancellationToken::new(),
            indexing_deadline: std::time::Duration::from_secs(25),
            background_indexing_limit: std::time::Duration::from_secs(600),
        }
    }
}

/// Current UTC time.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Tenant-only scope (child tables of an already authorized chat, workers).
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Tenant + owner scope (quota rows, reactions).
#[must_use]
pub fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(OWNER_TENANT_ID, tenant_id),
        ScopeFilter::eq(OWNER_ID, user_id),
    ]))
}

/// Fires every wake collected in a committed transaction.
pub fn fire(wakes: Vec<Wake>) {
    for w in wakes {
        w.fire();
    }
}
