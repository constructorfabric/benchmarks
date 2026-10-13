//! Repositories over the mini-chat tables.
//!
//! All methods take `runner: &impl DBRunner` (a `DbConn` or a `DbTx`, so they
//! compose inside transactions) and the caller's `&AccessScope` — normally the
//! tenant + owner scope compiled by the PEP for the chat owner.
//!
//! Owner-scoped tables (`chats`, `quota_usage`, `message_reactions`) apply that
//! scope as is. Child tables of a chat have no owner column, so their repos
//! narrow it with [`AccessScope::tenant_only`]: callers MUST have loaded the
//! parent chat with the full scope first (D§3.7 "Tenant scoping"); background
//! jobs pass `AccessScope::for_tenant(..)`.
//!
//! Unit structs; later tasks add the methods they need. Errors are
//! `ScopeError` (`is_unique_violation()` distinguishes idempotency conflicts).

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel};
use toolkit_db::secure::{AccessScope, DBRunner, ScopableEntity, ScopeError, secure_insert};

mod attachment_repo;
mod chat_repo;
mod message_attachment_repo;
mod message_repo;
mod quota_usage_repo;
mod reaction_repo;
mod thread_summary_repo;
mod turn_repo;
mod vector_store_repo;

pub use attachment_repo::AttachmentRepo;
pub use chat_repo::ChatRepo;
pub use message_attachment_repo::MessageAttachmentRepo;
pub use message_repo::{MessageRepo, OrderKey};
pub use quota_usage_repo::{BucketKey, QuotaIncrement, QuotaUsageRepo};
pub use reaction_repo::ReactionRepo;
pub use thread_summary_repo::ThreadSummaryRepo;
pub use turn_repo::{ToolCounter, TurnPreflight, TurnRepo, TurnTerminal};
pub use vector_store_repo::VectorStoreRepo;

/// Insert a complete model (every column `Set`) through the Secure ORM.
pub(crate) async fn insert_model<E>(
    runner: &impl DBRunner,
    scope: &AccessScope,
    model: E::Model,
) -> Result<E::Model, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Send,
    E::Model: IntoActiveModel<E::ActiveModel>,
{
    let am = model.into_active_model().reset_all();
    secure_insert::<E>(am, scope, runner).await
}
