//! Domain services (orchestration + PEP). All operations hang off the shared
//! [`AppState`].

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit_db::Db;
use toolkit_db::outbox::Outbox;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::infra::llm::LlmGateway;
use crate::infra::policy::{AuditGateway, PolicyGateway};

pub mod attachments;
pub mod chats;
pub mod cleanup;
pub mod finalize;
pub mod messages;
pub mod models;
pub mod mutations;
pub mod outbox;
pub mod provider_task;
pub mod quota;
pub mod reactions;
pub mod stream;
pub mod summary;
pub mod workers;

/// The outbox handle becomes available when the gear starts; enqueuers wait
/// for it briefly.
#[derive(Clone)]
pub struct OutboxSlot {
    rx: watch::Receiver<Option<Arc<Outbox>>>,
    tx: Arc<watch::Sender<Option<Arc<Outbox>>>>,
}

impl Default for OutboxSlot {
    fn default() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            rx,
            tx: Arc::new(tx),
        }
    }
}

impl OutboxSlot {
    pub fn set(&self, outbox: Option<Arc<Outbox>>) {
        self.tx.send_replace(outbox);
    }

    /// Wait (up to 15 s) for the outbox pipeline.
    ///
    /// # Errors
    /// 503 when the pipeline is not running.
    pub async fn get(&self) -> Result<Arc<Outbox>, DomainError> {
        let mut rx = self.rx.clone();
        if let Some(o) = rx.borrow().clone() {
            return Ok(o);
        }
        let wait = async {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                if let Some(o) = rx.borrow().clone() {
                    return Some(o);
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(15), wait).await {
            Ok(Some(o)) => Ok(o),
            _ => Err(DomainError::unavailable(5)),
        }
    }
}

/// Shared state of the gear.
pub struct AppState {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Db,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub hub: Arc<ClientHub>,
    pub llm: Arc<LlmGateway>,
    pub outbox: OutboxSlot,
    /// Cancelled when the gear stops (background tasks).
    pub cancel: CancellationToken,
    pub upload_sem: Arc<Semaphore>,
}

impl AppState {
    #[must_use]
    pub fn max_output_cap(&self) -> u32 {
        self.cfg.streaming.max_output_tokens
    }
}

pub type SharedState = Arc<AppState>;

/// Platform default (system) identity, used when no S2S identity exists.
#[must_use]
pub fn default_system_ctx() -> Option<toolkit_security::SecurityContext> {
    toolkit_security::SecurityContext::builder()
        .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
        .subject_tenant_id(toolkit_security::constants::DEFAULT_TENANT_ID)
        .build()
        .ok()
}

impl AppState {
    /// Startup check of the thread-summary model (logs only).
    pub async fn check_summary_model(&self) {
        let id = self
            .cfg
            .thread_summary_worker
            .effective_summary_model_id()
            .to_owned();
        for _ in 0..10 {
            match self
                .policy
                .current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID)
                .await
            {
                Ok(s) => {
                    if s.find_enabled(&id).is_none() {
                        tracing::error!(model = %id, "thread summary model is missing or disabled in the catalog");
                    }
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }
}

use std::future::Future;
use std::pin::Pin;
use toolkit_db::DbTx;
use toolkit_db::secure::TxConfig;

impl AppState {
    /// Run a write transaction, retrying on backend contention.
    ///
    /// # Errors
    /// The closure's error or a database error.
    pub async fn write_tx<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnMut(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
            + Send,
    {
        self.db
            .transaction_with_retry(TxConfig::default(), |e: &DomainError| e.db_err(), f)
            .await
    }

    /// Non-transactional connection.
    ///
    /// # Errors
    /// When a connection cannot be obtained.
    pub fn conn(&self) -> Result<toolkit_db::DbConn<'_>, DomainError> {
        self.db.conn().map_err(DomainError::from)
    }
}
