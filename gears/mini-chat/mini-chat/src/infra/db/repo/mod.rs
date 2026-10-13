//! Secure ORM repositories of the domain persistence ports
//! (`crate::domain::ports`).
//!
//! One module per table family. Each module exposes free functions over any
//! [`DBRunner`] (a connection or a transaction), so later slices compose them
//! inside their own transactions, and implements its port on [`DbRepository`]
//! for the domain services:
//!
//! - [`chats`] — `chats` reads and writes, the batch message count and the
//!   soft-delete transaction ([`crate::domain::ports::ChatRepository`]);
//! - [`messages`] — `messages` reads, the context queries, the thread
//!   summary read and the turn message inserts
//!   ([`crate::domain::ports::MessageRepository`]);
//! - [`attachments`] — attachment summaries, cleanup marks, the stream's
//!   attachment reads and message links
//!   ([`crate::domain::ports::AttachmentRepository`]), the attachment
//!   service's writes ([`crate::domain::ports::AttachmentStore`]) and the
//!   attachment cleanup bookkeeping;
//! - [`vector_stores`] — `chat_vector_stores` creation protocol rows
//!   ([`crate::domain::ports::VectorStoreRepository`]);
//! - [`reactions`] — `message_reactions` ([`crate::domain::ports::ReactionRepository`]);
//! - [`summaries`] — the thread-summary range and the CAS commit
//!   ([`crate::domain::ports::ThreadSummaryStore`]);
//! - [`turns`] — `chat_turns` reads, the running-turn insert, the
//!   finalization CAS and progress updates
//!   ([`crate::domain::ports::TurnRepository`]), and the finalization outbox
//!   producer ([`crate::domain::ports::TurnOutbox`]);
//! - [`quota`] — `quota_usage` bucket reads, reserve and settlement upserts
//!   ([`crate::domain::ports::QuotaRepository`]);
//! - [`odata`] — `OData` field mappings and query normalization;
//! - [`paging`] — keyset pagination over NULL-safe sort keys (chat list).
//!
//! **Scopes.** The chat queries apply the PEP scope as is. Child tables have
//! no owner column, so their queries use [`child_scope`] (the tenant
//! predicates of the chat scope) plus an explicit `chat_id` predicate, after
//! the parent chat was loaded with the full scope; `message_reactions` uses
//! [`owner_scope`] (tenant + owner) and an explicit `user_id` predicate.
//!
//! **Timestamps.** Free functions that write or compare timestamp columns
//! take the [`TimestampForm`] of the database ([`DbRepository::timestamps`])
//! and bind every timestamp through it (see
//! [`crate::infra::db::entities`]), so `SQLite` rows keep the fixed-width
//! text form.
//!
//! [`DBRunner`]: toolkit_db::secure::DBRunner

use std::sync::Arc;

use toolkit_db::{DBProvider, DbError};
use toolkit_security::AccessScope;

use crate::domain::ports::Repositories;
use crate::infra::db::entities::TimestampForm;
use crate::infra::outbox::OutboxEnqueuer;

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod odata;
pub mod paging;
pub mod quota;
pub mod reactions;
pub mod summaries;
pub mod turns;
pub mod vector_stores;

/// Database provider of the gear.
pub type GearDb = DBProvider<DbError>;

/// The Secure ORM implementation of every persistence port.
#[derive(Clone)]
pub struct DbRepository {
    db: Arc<GearDb>,
    outbox: Arc<OutboxEnqueuer>,
    timestamps: TimestampForm,
}

impl std::fmt::Debug for DbRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbRepository")
            .field("outbox", &self.outbox)
            .field("timestamps", &self.timestamps)
            .finish_non_exhaustive()
    }
}

impl DbRepository {
    /// Repositories over the gear database; `outbox` produces the messages
    /// enqueued by repository transactions (chat cleanup).
    #[must_use]
    pub fn new(db: Arc<GearDb>, outbox: Arc<OutboxEnqueuer>) -> Self {
        let timestamps = TimestampForm::of_backend(db.db().backend());
        Self {
            db,
            outbox,
            timestamps,
        }
    }

    /// The database provider.
    #[must_use]
    pub fn db(&self) -> &Arc<GearDb> {
        &self.db
    }

    /// The outbox producer.
    #[must_use]
    pub fn outbox(&self) -> &Arc<OutboxEnqueuer> {
        &self.outbox
    }

    /// How timestamp columns are bound on the gear database.
    #[must_use]
    pub const fn timestamps(&self) -> TimestampForm {
        self.timestamps
    }

    /// Every port backed by `self`.
    #[must_use]
    pub fn repositories(self: &Arc<Self>) -> Repositories {
        Repositories {
            chats: Arc::clone(self) as _,
            messages: Arc::clone(self) as _,
            attachments: Arc::clone(self) as _,
            reactions: Arc::clone(self) as _,
            turns: Arc::clone(self) as _,
            outbox: Arc::clone(self) as _,
        }
    }
}

/// Scope of a child table of a chat already loaded through `chat_scope`: the
/// tenant predicates only (child rows have no owner or chat-id resource
/// column). Must be combined with the parent `chat_id` predicate.
#[must_use]
pub fn child_scope(chat_scope: &AccessScope) -> AccessScope {
    chat_scope.tenant_only()
}

/// Scope of an owner-scoped child table (`message_reactions`): the tenant and
/// owner predicates of the chat scope.
#[must_use]
pub fn owner_scope(chat_scope: &AccessScope) -> AccessScope {
    chat_scope.tenant_and_owner()
}

#[cfg(test)]
#[path = "repo_tests.rs"]
pub(crate) mod tests;
