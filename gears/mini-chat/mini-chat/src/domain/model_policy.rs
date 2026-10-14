//! Port to the model-policy plugin (`MiniChatModelPolicyPluginClientV1`).
//!
//! The gear keeps no snapshot cache, no snapshot table and no notify endpoint
//! (ADR-0008): every call asks the plugin.

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, UserLimits};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// The model-policy gateway: policy version, snapshot and user limits.
#[async_trait]
pub trait ModelPolicyGateway: Send + Sync {
    /// The snapshot of the current policy version (asks the plugin for the version,
    /// then for the snapshot of that version).
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyUnavailable`] when the plugin cannot be resolved or fails.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>;

    /// The snapshot of a given version (settlement reads the turn's `policy_version_applied`).
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyUnavailable`] when the plugin cannot be resolved or fails.
    async fn snapshot_of_version(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError>;

    /// The per-user limits under `policy_version`. Never merged into the snapshot.
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyUnavailable`] when the plugin cannot be resolved or fails.
    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError>;
}
