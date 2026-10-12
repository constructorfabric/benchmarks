//! Plugin contracts implemented by mini-chat plugins and resolved by the gear
//! through types-registry + scoped `ClientHub` (see [`crate::gts`]).
//!
//! - [`MiniChatModelPolicyPluginClientV1`] — policy snapshots, per-user limits
//!   and usage publication (DESIGN section 5.2). Bundled implementation:
//!   `static-mini-chat-model-policy-plugin`.
//! - [`MiniChatAuditPluginClientV1`] — audit event delivery (DESIGN section
//!   3.2 "Audit plugin and audit outbox", ADR-0009). Bundled implementation:
//!   `static-mini-chat-audit-plugin`.
//!
//! Error classification drives the outbox handler outcome (DESIGN section 5.6
//! "Shared Outbox Processing Model"): transient errors map to `Retry`,
//! permanent errors to `Reject`.

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

use crate::models::{AuditEvent, PolicySnapshot, UsageEvent, UserLicenseStatus, UserLimits};

/// Error returned by the read side of [`MiniChatModelPolicyPluginClientV1`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MiniChatModelPolicyPluginError {
    /// The plugin does not serve the requested policy version.
    #[error("policy version {policy_version} not found")]
    VersionNotFound {
        /// Requested policy version.
        policy_version: u64,
    },
    /// No policy data exists for the user.
    #[error("no policy data for user {user_id}")]
    UserNotFound {
        /// Requested user.
        user_id: Uuid,
    },
    /// The backing policy source is temporarily unavailable; retrying may succeed.
    #[error("policy plugin temporarily unavailable: {0}")]
    Transient(String),
    /// Unexpected plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

impl MiniChatModelPolicyPluginError {
    /// Whether retrying the same call may succeed.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Error returned by [`MiniChatModelPolicyPluginClientV1::publish_usage`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    /// Delivery failed but may succeed later; the usage outbox handler returns `Retry`.
    #[error("usage publication failed transiently: {0}")]
    Transient(String),
    /// The event can never be accepted; the usage outbox handler returns `Reject`.
    #[error("usage publication rejected: {0}")]
    Permanent(String),
}

impl PublishError {
    /// Whether retrying the publication may succeed.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Error returned by [`MiniChatAuditPluginClientV1::emit`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MiniChatAuditPluginError {
    /// Delivery failed but may succeed later; the audit outbox handler returns `Retry`.
    #[error("audit delivery failed transiently: {0}")]
    Transient(String),
    /// The plugin call exceeded the handler timeout (transient, `Retry`).
    #[error("audit plugin call timed out")]
    PluginTimeout,
    /// The event can never be accepted; the audit outbox handler returns `Reject`.
    #[error("audit event rejected: {0}")]
    Permanent(String),
}

impl MiniChatAuditPluginError {
    /// Whether retrying the delivery may succeed (`Transient` or `PluginTimeout`).
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::PluginTimeout)
    }
}

/// Model policy plugin contract (DESIGN section 5.2.2).
///
/// The plugin is "dumb": it returns versioned configuration and receives
/// usage events. Implementations are registered as scoped clients under the
/// GTS instance id of a [`MiniChatModelPolicyPluginSpecV1`](crate::MiniChatModelPolicyPluginSpecV1)
/// plugin instance.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Returns the current policy version for the user's tenant.
    ///
    /// # Errors
    ///
    /// [`MiniChatModelPolicyPluginError::UserNotFound`] when the plugin has no
    /// policy for the user, [`MiniChatModelPolicyPluginError::Transient`] when
    /// the policy source is unavailable, and
    /// [`MiniChatModelPolicyPluginError::Internal`] on unexpected failures.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError>;

    /// Returns the immutable policy snapshot of `policy_version`.
    ///
    /// # Errors
    ///
    /// [`MiniChatModelPolicyPluginError::VersionNotFound`] when the version is
    /// not served, [`MiniChatModelPolicyPluginError::Transient`] when the
    /// policy source is unavailable, and
    /// [`MiniChatModelPolicyPluginError::Internal`] on unexpected failures.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Returns the user's credit limits keyed by `policy_version`.
    ///
    /// # Errors
    ///
    /// [`MiniChatModelPolicyPluginError::VersionNotFound`] when the version is
    /// not served, [`MiniChatModelPolicyPluginError::UserNotFound`] when no
    /// allocation exists for the user,
    /// [`MiniChatModelPolicyPluginError::Transient`] when the policy source is
    /// unavailable, and [`MiniChatModelPolicyPluginError::Internal`] on
    /// unexpected failures.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publishes one usage event (at-least-once; consumers deduplicate by
    /// [`UsageEvent::dedupe_key`]).
    ///
    /// # Errors
    ///
    /// [`PublishError::Transient`] when delivery may succeed on retry and
    /// [`PublishError::Permanent`] when the event can never be accepted.
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// Checks whether the user holds an active mini-chat license.
    ///
    /// The gear does not call this method (the license gate is enforced on the
    /// routes); the default body reports `active: false`.
    ///
    /// # Errors
    ///
    /// The default body never fails. Overrides return
    /// [`MiniChatModelPolicyPluginError::Transient`] or
    /// [`MiniChatModelPolicyPluginError::Internal`] when the license source
    /// cannot be queried.
    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        let _ = user_id;
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin contract (DESIGN section 3.2, ADR-0009).
///
/// Implementations are registered as scoped clients under the GTS instance id
/// of a [`MiniChatAuditPluginSpecV1`](crate::MiniChatAuditPluginSpecV1) plugin
/// instance.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Delivers one audit event.
    ///
    /// # Errors
    ///
    /// [`MiniChatAuditPluginError::Transient`] or
    /// [`MiniChatAuditPluginError::PluginTimeout`] when delivery may succeed on
    /// retry, [`MiniChatAuditPluginError::Permanent`] when the event can never
    /// be accepted.
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoLicenseOverride;

    #[async_trait]
    impl MiniChatModelPolicyPluginClientV1 for NoLicenseOverride {
        async fn get_current_policy_version(
            &self,
            _user_id: Uuid,
        ) -> Result<u64, MiniChatModelPolicyPluginError> {
            Ok(1)
        }

        async fn get_policy_snapshot(
            &self,
            _user_id: Uuid,
            policy_version: u64,
        ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
            Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
        }

        async fn get_user_limits(
            &self,
            _user_id: Uuid,
            policy_version: u64,
        ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
            Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
        }

        async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn check_user_license_default_reports_inactive() {
        let status = NoLicenseOverride
            .check_user_license(Uuid::nil())
            .await
            .expect("default body never fails");

        assert!(!status.active);
    }

    #[test]
    fn error_transience_classification() {
        assert!(MiniChatAuditPluginError::PluginTimeout.is_transient());
        assert!(MiniChatAuditPluginError::Transient("x".to_owned()).is_transient());
        assert!(!MiniChatAuditPluginError::Permanent("x".to_owned()).is_transient());
        assert!(PublishError::Transient("x".to_owned()).is_transient());
        assert!(!PublishError::Permanent("x".to_owned()).is_transient());
        assert!(MiniChatModelPolicyPluginError::Transient("x".to_owned()).is_transient());
        assert!(
            !MiniChatModelPolicyPluginError::VersionNotFound { policy_version: 2 }.is_transient()
        );
    }
}
