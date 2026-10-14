#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{AuditPluginError, ModelPolicyPluginError, PublishError};
use crate::models::{AuditEvent, PolicySnapshot, UsageEvent, UserLimits};
use crate::plugin_api::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};

struct MinimalPolicyPlugin;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for MinimalPolicyPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError> {
        Ok(1)
    }
    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::PolicyVersionNotFound(
            policy_version,
        ))
    }
    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::PolicyVersionNotFound(
            policy_version,
        ))
    }
    async fn publish_usage(&self, _event: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

struct SilentAudit;

#[async_trait]
impl MiniChatAuditPluginClientV1 for SilentAudit {
    async fn emit(&self, _event: AuditEvent) -> Result<(), AuditPluginError> {
        Err(AuditPluginError::PluginTimeout)
    }
}

#[tokio::test]
async fn check_user_license_default_body_returns_inactive() {
    let status = MinimalPolicyPlugin
        .check_user_license(Uuid::from_u128(1))
        .await
        .unwrap();
    assert!(!status.active);
}

#[test]
fn plugin_error_messages_do_not_hide_the_category() {
    assert!(
        PublishError::Transient("x".to_owned())
            .to_string()
            .contains("transient")
    );
    assert!(
        PublishError::Permanent("x".to_owned())
            .to_string()
            .contains("permanent")
    );
    assert!(
        AuditPluginError::PluginTimeout
            .to_string()
            .contains("timed out")
    );
    assert!(
        ModelPolicyPluginError::PolicyVersionNotFound(9)
            .to_string()
            .contains('9')
    );
}

#[tokio::test]
async fn audit_plugin_trait_is_object_safe_and_reports_timeouts() {
    let plugin: std::sync::Arc<dyn MiniChatAuditPluginClientV1> = std::sync::Arc::new(SilentAudit);
    let event = crate::models::AuditEvent::TurnDelete(crate::models::TurnDeleteAuditEvent {
        actor_user_id: Uuid::from_u128(1),
        chat_id: Uuid::from_u128(2),
        request_id: Uuid::from_u128(3),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
    });
    assert_eq!(
        plugin.emit(event).await.unwrap_err(),
        AuditPluginError::PluginTimeout
    );
}
