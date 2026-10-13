//! Recording plugin fakes: [`RecordingPolicy`] and [`RecordingAudit`] implement the SDK plugin
//! traits, record every call and can be scripted (state changes, failures).

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, KillSwitches, MiniChatAuditPluginClientV1,
    MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, PolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// The only policy version [`RecordingPolicy`] serves.
const POLICY_VERSION: i64 = 1;

/// One call received by [`RecordingPolicy`].
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyCall {
    CurrentVersion { user_id: Uuid },
    Snapshot { user_id: Uuid, version: i64 },
    Limits { user_id: Uuid, version: i64 },
    PublishUsage(Box<UsageEvent>),
}

struct PolicyState {
    catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
    standard: TierLimits,
    premium: TierLimits,
    fail_snapshots: bool,
    fail_next_snapshots: usize,
    publish_results: VecDeque<PublishError>,
}

/// Model policy plugin fake serving the current catalog, kill switches and limits as policy
/// version 1 (other versions are `NotFound`), recording every call.
///
/// Unlike the static plugin it does not validate the catalog, so tests can serve entries the gear
/// must tolerate. Everything can change after build (`set_*`); the version stays 1.
/// `get_policy_snapshot` fails with `PolicyPluginError::Unavailable` when scripted
/// ([`Self::fail_next_snapshot`], [`Self::fail_snapshots`]); `publish_usage` succeeds unless
/// failures were queued with [`Self::fail_next_publish`].
pub struct RecordingPolicy {
    state: Mutex<PolicyState>,
    calls: Mutex<Vec<PolicyCall>>,
    generated_at: OffsetDateTime,
}

impl RecordingPolicy {
    /// Serves `catalog` (in order), `kill_switches` and the given standard / premium limits.
    pub fn new(
        catalog: Vec<ModelCatalogEntry>,
        kill_switches: KillSwitches,
        standard: TierLimits,
        premium: TierLimits,
    ) -> Self {
        Self {
            state: Mutex::new(PolicyState {
                catalog,
                kill_switches,
                standard,
                premium,
                fail_snapshots: false,
                fail_next_snapshots: 0,
                publish_results: VecDeque::new(),
            }),
            calls: Mutex::new(Vec::new()),
            generated_at: OffsetDateTime::now_utc(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PolicyState> {
        self.state.lock().expect("lock")
    }

    /// Replaces the catalog served from now on.
    pub fn set_catalog(&self, catalog: Vec<ModelCatalogEntry>) {
        self.state().catalog = catalog;
    }

    /// Replaces the kill switches served from now on.
    pub fn set_kill_switches(&self, kill_switches: KillSwitches) {
        self.state().kill_switches = kill_switches;
    }

    /// Replaces the standard / premium limits returned from now on.
    pub fn set_limits(&self, standard: TierLimits, premium: TierLimits) {
        let mut state = self.state();
        state.standard = standard;
        state.premium = premium;
    }

    /// The next `get_policy_snapshot` call fails (calls accumulate: one failure per call).
    pub fn fail_next_snapshot(&self) {
        self.state().fail_next_snapshots += 1;
    }

    /// Every `get_policy_snapshot` call fails while `fail` is true.
    pub fn fail_snapshots(&self, fail: bool) {
        self.state().fail_snapshots = fail;
    }

    /// The next `publish_usage` call fails with `err` (queued; one error per call).
    pub fn fail_next_publish(&self, err: PublishError) {
        self.state().publish_results.push_back(err);
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<PolicyCall> {
        self.calls.lock().expect("lock").clone()
    }

    /// Every event passed to `publish_usage` (including failed attempts), in order.
    pub fn usage_events(&self) -> Vec<UsageEvent> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                PolicyCall::PublishUsage(ev) => Some(*ev),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: PolicyCall) {
        self.calls.lock().expect("lock").push(call);
    }

    fn check_version(version: i64) -> Result<(), PolicyPluginError> {
        if version == POLICY_VERSION {
            Ok(())
        } else {
            Err(PolicyPluginError::NotFound(format!(
                "policy version {version}"
            )))
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for RecordingPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        self.record(PolicyCall::CurrentVersion { user_id });
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        self.record(PolicyCall::Snapshot {
            user_id,
            version: policy_version,
        });
        let mut state = self.state();
        if state.fail_snapshots || state.fail_next_snapshots > 0 {
            state.fail_next_snapshots = state.fail_next_snapshots.saturating_sub(1);
            return Err(PolicyPluginError::Unavailable(
                "scripted snapshot failure".to_owned(),
            ));
        }
        Self::check_version(policy_version)?;
        Ok(PolicySnapshot {
            policy_version: POLICY_VERSION,
            model_catalog: state.catalog.clone(),
            kill_switches: state.kill_switches,
        })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, PolicyPluginError> {
        self.record(PolicyCall::Limits {
            user_id,
            version: policy_version,
        });
        Self::check_version(policy_version)?;
        let state = self.state();
        Ok(UserLimits {
            user_id,
            policy_version: POLICY_VERSION,
            standard: state.standard,
            premium: state.premium,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        self.record(PolicyCall::PublishUsage(Box::new(event)));
        match self.state().publish_results.pop_front() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// Audit plugin fake: records every emitted event; succeeds unless failures were queued with
/// [`Self::fail_next`].
#[derive(Default)]
pub struct RecordingAudit {
    events: Mutex<Vec<AuditEvent>>,
    results: Mutex<VecDeque<AuditPluginError>>,
}

impl RecordingAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event passed to `emit` (including failed attempts), in order.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("lock").clone()
    }

    /// The next `emit` call fails with `err` (queued; one error per call).
    pub fn fail_next(&self, err: AuditPluginError) {
        self.results.lock().expect("lock").push_back(err);
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        self.events.lock().expect("lock").push(event);
        match self.results.lock().expect("lock").pop_front() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::{
        KillSwitches, MiniChatModelPolicyPluginClientV1, PolicyPluginError, TierLimits,
    };
    use uuid::Uuid;

    use super::*;
    use crate::test_support::app::{NO_KILL_SWITCHES, PREMIUM_LIMITS, STANDARD_LIMITS};
    use crate::test_support::catalog::{standard_model, test_catalog};

    #[tokio::test]
    async fn policy_state_can_change_after_build_and_stays_version_1() {
        let policy = RecordingPolicy::new(
            test_catalog(),
            NO_KILL_SWITCHES,
            STANDARD_LIMITS,
            PREMIUM_LIMITS,
        );
        let user = Uuid::new_v4();

        // A catalog the static plugin would reject (zero multiplier) is served as is.
        let mut odd = standard_model("odd");
        odd.input_tokens_credit_multiplier_micro = 0;
        policy.set_catalog(vec![odd.clone()]);
        let switches = KillSwitches {
            force_standard_tier: true,
            ..NO_KILL_SWITCHES
        };
        policy.set_kill_switches(switches);
        let small = TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 2,
        };
        policy.set_limits(small, STANDARD_LIMITS);

        assert_eq!(
            policy
                .get_current_policy_version(user)
                .await
                .unwrap()
                .policy_version,
            1
        );
        let snapshot = policy.get_policy_snapshot(user, 1).await.unwrap();
        assert_eq!(snapshot.policy_version, 1);
        assert_eq!(snapshot.model_catalog, [odd]);
        assert_eq!(snapshot.kill_switches, switches);
        let limits = policy.get_user_limits(user, 1).await.unwrap();
        assert_eq!(
            (
                limits.user_id,
                limits.policy_version,
                limits.standard,
                limits.premium
            ),
            (user, 1, small, STANDARD_LIMITS)
        );
        assert!(matches!(
            policy.get_policy_snapshot(user, 2).await,
            Err(PolicyPluginError::NotFound(_))
        ));
        assert!(matches!(
            policy.get_user_limits(user, 2).await,
            Err(PolicyPluginError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn snapshot_failures_are_scriptable() {
        let policy = RecordingPolicy::new(
            test_catalog(),
            NO_KILL_SWITCHES,
            STANDARD_LIMITS,
            PREMIUM_LIMITS,
        );
        let user = Uuid::new_v4();
        let unavailable =
            |r: Result<_, PolicyPluginError>| matches!(r, Err(PolicyPluginError::Unavailable(_)));

        policy.fail_next_snapshot();
        assert!(unavailable(policy.get_policy_snapshot(user, 1).await));
        assert!(
            policy.get_policy_snapshot(user, 1).await.is_ok(),
            "one-shot"
        );

        policy.fail_snapshots(true);
        assert!(unavailable(policy.get_policy_snapshot(user, 1).await));
        assert!(unavailable(policy.get_policy_snapshot(user, 1).await));
        policy.fail_snapshots(false);
        assert!(policy.get_policy_snapshot(user, 1).await.is_ok());
    }
}
