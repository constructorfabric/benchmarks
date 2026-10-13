//! Repositories: free functions over `&impl DBRunner` that own the queries.
//!
//! Chat-level queries take the caller's owner-scoped [`AccessScope`]. Child
//! tables (messages, turns, reactions, attachments) are only queried after the
//! parent chat was authorized; they filter by `chat_id` under a tenant scope
//! that resolves on the child entity (DESIGN section 3.7, "Secure ORM").

use toolkit_db::secure::AccessScope;
use uuid::Uuid;

pub mod attachment_repo;
pub mod chat_repo;
pub mod message_repo;
pub mod quota_repo;
pub mod reaction_repo;
pub mod turn_repo;
pub mod vector_store_repo;

/// Tenant-only scope for child rows of an already authorized chat.
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

#[cfg(test)]
#[path = "repos_tests.rs"]
mod repos_tests;
