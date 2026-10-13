//! Policy resolution (`cpt-cf-mini-chat-algo-quota-billing-resolve-policy`,
//! `cpt-cf-mini-chat-dod-quota-billing-policy-binding`): the current policy
//! version, the snapshot and the user limits at preflight, the limits at
//! status, and the snapshot of a given version at settlement. Nothing is
//! cached: every call repeats the plugin calls.

use mini_chat_sdk::{PolicySnapshot, UserLimits};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;

/// Result of the preflight resolution.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightPolicy {
    /// The current policy version, bound to the turn.
    pub policy_version: u64,
    /// The snapshot of that version.
    pub snapshot: PolicySnapshot,
    /// The user limits of that version.
    pub limits: UserLimits,
}

/// Result of the status resolution (no snapshot is read).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusPolicy {
    /// The current policy version.
    pub policy_version: u64,
    /// The user limits of that version.
    pub limits: UserLimits,
}

/// The snapshot of the turn's `policy_version_applied`, fetched before the
/// finalization transaction opens.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum SettlementSnapshot {
    /// The snapshot was fetched.
    Available(Box<PolicySnapshot>),
    /// The version is NULL, or the fetch failed or timed out.
    Unavailable,
}

impl SettlementSnapshot {
    /// The snapshot when it is available and its `policy_version` equals
    /// `policy_version_applied`; `None` otherwise (the settlement then takes
    /// the deterministic fallback).
    #[must_use]
    pub fn usable_for(&self, policy_version_applied: u64) -> Option<&PolicySnapshot> {
        match self {
            Self::Available(snapshot) if snapshot.policy_version == policy_version_applied => {
                Some(snapshot)
            }
            _ => None,
        }
    }
}

/// Preflight mode: current version, then the snapshot and the limits of that
/// version.
///
/// # Errors
///
/// [`DomainError::PolicyResolution`] when the plugin is missing, fails or
/// times out at any of the three calls (no partial result is used).
// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-policy-binding:p1
pub async fn resolve_preflight_policy(
    policy: &dyn PolicyCatalog,
    user_id: Uuid,
) -> Result<PreflightPolicy, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-no-cache
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-resolve-plugin
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-version
    let version = policy.current_policy_version(user_id).await?.policy_version;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-version
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-resolve-plugin
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-preflight-mode
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-snapshot
    let snapshot = policy.policy_snapshot(user_id, version).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-snapshot
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-preflight-mode
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-user-limits
    let limits = policy.user_limits(user_id, version).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-user-limits
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-no-cache
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy
    Ok(PreflightPolicy {
        policy_version: version,
        snapshot,
        limits,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy
}

/// Status mode: current version, then the limits of that version.
///
/// # Errors
///
/// [`DomainError::PolicyResolution`] when the plugin is missing, fails or
/// times out.
pub async fn resolve_status_policy(
    policy: &dyn PolicyCatalog,
    user_id: Uuid,
) -> Result<StatusPolicy, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-no-cache
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-resolve-plugin
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-version
    let version = policy.current_policy_version(user_id).await?.policy_version;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-current-version
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-resolve-plugin
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-user-limits
    let limits = policy.user_limits(user_id, version).await?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-user-limits
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-no-cache
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy
    Ok(StatusPolicy {
        policy_version: version,
        limits,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy
}

/// Settlement mode: the snapshot of `policy_version_applied`, never the
/// current version. A NULL version is not fetched.
///
/// Never fails: a plugin error or timeout is logged at `error` with the turn
/// id and the cause and becomes [`SettlementSnapshot::Unavailable`].
pub async fn prefetch_settlement_snapshot(
    policy: &dyn PolicyCatalog,
    user_id: Uuid,
    turn_id: Uuid,
    policy_version_applied: Option<u64>,
) -> SettlementSnapshot {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-settlement-mode
    let Some(version) = policy_version_applied else {
        return SettlementSnapshot::Unavailable;
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-snapshot-by-version
    let fetched = policy.policy_snapshot(user_id, version).await;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-snapshot-by-version
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-settlement-snapshot
    match fetched {
        Ok(snapshot) => SettlementSnapshot::Available(Box::new(snapshot)),
        Err(error) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-snapshot-unavailable
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
            tracing::error!(
                turn_id = %turn_id,
                policy_version_applied = version,
                %error,
                "settlement snapshot unavailable, the deterministic estimated fallback applies"
            );
            SettlementSnapshot::Unavailable
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-policy-error
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-snapshot-unavailable
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-policy-failed
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-return-settlement-snapshot
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-resolve-policy:p1:inst-if-settlement-mode
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
