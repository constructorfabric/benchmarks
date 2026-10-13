#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the plugin API default bodies.

use async_trait::async_trait;
use uuid::Uuid;

use crate::models::{PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLimits};
use crate::plugin_api::{MiniChatModelPolicyPluginClientV1, PolicyPluginError, PublishError};

/// Plugin that implements only the required methods.
struct MinimalPlugin;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for MinimalPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        Err(PolicyPluginError::Internal("unused".to_owned()))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        Err(PolicyPluginError::Internal("unused".to_owned()))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError> {
        Err(PolicyPluginError::Internal("unused".to_owned()))
    }

    async fn publish_usage(&self, _event: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

#[tokio::test]
async fn default_check_user_license_reports_inactive() {
    let status = MinimalPlugin
        .check_user_license(Uuid::from_u128(1))
        .await
        .expect("default body succeeds");

    assert!(!status.active);
}

#[test]
fn publish_error_distinguishes_transient_from_permanent() {
    assert_ne!(
        PublishError::Transient("x".to_owned()),
        PublishError::Permanent("x".to_owned())
    );
}
