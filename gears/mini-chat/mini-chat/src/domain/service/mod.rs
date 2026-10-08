//! Domain services: orchestration, PEP, quota, streaming and persistence.

pub mod attachments;
pub mod authz;
pub mod chats;
pub mod finalize;
pub mod messages;
pub mod models;
pub mod policy;
pub mod quota;
pub mod stream;
pub mod summary;
pub mod turns;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::Db;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::infra::llm::transport::{ProviderTransport, StorageClient};
use crate::infra::outbox::OutboxEnqueuer;

pub use authz::Authz;

/// Take the database write lock as the transaction's first statement.
///
/// `SQLite` (WAL) opens transactions as `DEFERRED`: a transaction that reads
/// before it writes fails immediately with `SQLITE_BUSY_SNAPSHOT` when another
/// connection committed in between, without honouring `busy_timeout`. A no-op
/// write up front gives `BEGIN IMMEDIATE` semantics (wait for the lock under
/// `busy_timeout`). On server databases it matches no rows and takes no locks.
///
/// # Errors
/// Database errors (including [`DomainError::Contention`] on lock timeout).
pub async fn lock_for_write(tx: &toolkit_db::secure::DbTx<'_>) -> Result<(), DomainError> {
    use sea_orm::sea_query::Expr;
    use sea_orm::{EntityTrait, QueryFilter};
    use toolkit_db::secure::SecureUpdateExt;
    use toolkit_security::AccessScope;

    use crate::infra::db::entity::chats;

    chats::Entity::update_many()
        .col_expr(chats::Column::Id, Expr::col(chats::Column::Id))
        .filter(Expr::cust("1 = 0"))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await?;
    Ok(())
}
pub use policy::PolicySource;

/// Shared dependencies of every service.
pub struct AppServices {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Db,
    pub authz: Authz,
    pub policy: Arc<dyn PolicySource>,
    pub transport: Arc<dyn ProviderTransport>,
    pub storage: StorageClient,
    pub outbox: Arc<OutboxEnqueuer>,
    pub upload_slots: Arc<Semaphore>,
    /// Cancelled on gear stop (background indexing waits).
    pub shutdown: CancellationToken,
}

impl AppServices {
    #[must_use]
    pub fn new(
        cfg: Arc<MiniChatConfig>,
        db: Db,
        authz: Authz,
        policy: Arc<dyn PolicySource>,
        transport: Arc<dyn ProviderTransport>,
        outbox: Arc<OutboxEnqueuer>,
    ) -> Arc<Self> {
        let slots = usize::from(cfg.rag.max_concurrent_uploads);
        Arc::new(Self {
            storage: StorageClient {
                transport: Arc::clone(&transport),
            },
            cfg,
            db,
            authz,
            policy,
            transport,
            outbox,
            upload_slots: Arc::new(Semaphore::new(slots)),
            shutdown: CancellationToken::new(),
        })
    }

    /// Non-transactional runner.
    ///
    /// # Errors
    /// Called inside a transaction.
    pub fn conn(&self) -> Result<toolkit_db::DbConn<'_>, DomainError> {
        self.db.conn().map_err(DomainError::from)
    }
}

/// Current time truncated to microseconds (stable round-trips in both engines).
#[must_use]
pub fn now() -> DateTime<Utc> {
    let n = Utc::now();
    let micros = n.timestamp_micros();
    DateTime::<Utc>::from_timestamp_micros(micros).unwrap_or(n)
}
