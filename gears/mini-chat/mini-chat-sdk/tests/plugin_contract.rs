//! Contract of the two plugin client traits and their GTS plugin specs:
//! `MiniChatModelPolicyPluginClientV1` (model-policy plugin) and
//! `MiniChatAuditPluginClientV1` (audit plugin).
//!
//! Source of truth: DESIGN 3.2 ("Model policy gateway", "Audit plugin and audit
//! outbox"), 3.4, 5.2.2 (`check_user_license`: "default body returns
//! `active: false`") and 5.6 (permanent vs. transient plugin errors).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use toolkit::gts::PluginV1;
use uuid::Uuid;

/// Implements only the required methods; `check_user_license` must come from
/// the trait's default body.
struct MinimalPolicyPlugin;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for MinimalPolicyPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("unused".to_owned()))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("unused".to_owned()))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("unused".to_owned()))
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

struct CountingAuditPlugin {
    calls: AtomicUsize,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for CountingAuditPlugin {
    async fn emit(&self, _event: AuditEvent) -> Result<(), AuditPluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn check_user_license_defaults_to_inactive() {
    let plugin = MinimalPolicyPlugin;
    let status = plugin
        .check_user_license(Uuid::new_v4())
        .await
        .expect("the default body never fails");
    assert!(
        !status.active,
        "default check_user_license reports `active: false` (DESIGN 5.2.2)"
    );
}

#[tokio::test]
async fn plugin_clients_are_object_safe_and_shareable() {
    let policy: Arc<dyn MiniChatModelPolicyPluginClientV1> = Arc::new(MinimalPolicyPlugin);
    let audit = Arc::new(CountingAuditPlugin {
        calls: AtomicUsize::new(0),
    });
    let audit_dyn: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();

    assert!(
        policy
            .get_current_policy_version(Uuid::nil())
            .await
            .is_err()
    );
    let event: AuditEvent = serde_json::from_value(serde_json::json!({
        "event_type": "turn_delete",
        "tenant_id": Uuid::new_v4(),
        "actor_user_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "request_id": Uuid::new_v4(),
        "timestamp": "2026-03-02T10:00:00Z",
    }))
    .expect("turn_delete audit event");
    audit_dyn.emit(event).await.expect("emit");
    assert_eq!(audit.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn plugin_errors_distinguish_transient_from_permanent() {
    // DESIGN 5.6: usage handler maps a transient `PublishError` to Retry and a
    // permanent one to Reject; the audit handler maps `Transient` and
    // `PluginTimeout` to Retry and `Permanent` to Reject.
    assert!(PublishError::Transient("x".to_owned()).is_transient());
    assert!(!PublishError::Permanent("x".to_owned()).is_transient());

    assert!(AuditPluginError::Transient("x".to_owned()).is_transient());
    assert!(AuditPluginError::PluginTimeout.is_transient());
    assert!(!AuditPluginError::Permanent("x".to_owned()).is_transient());
}

#[test]
fn model_policy_plugin_spec_registers_under_the_mini_chat_plugin_namespace() {
    let (id, payload) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        "cf.core._.static_mini_chat_model_policy.v1",
        "constructorfabric",
        100,
    )
    .expect("registration payload builds");
    let id: &str = id.as_ref();
    assert!(
        id.starts_with("gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat."),
        "unexpected plugin type id: {id}"
    );
    assert!(
        id.contains("model_policy"),
        "unexpected plugin type id: {id}"
    );
    assert_eq!(payload["vendor"], "constructorfabric");
    assert_eq!(payload["priority"], 100);
}

#[test]
fn audit_plugin_spec_registers_under_the_mini_chat_plugin_namespace() {
    let (id, payload) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
        "cf.core._.static_mini_chat_audit.v1",
        "constructorfabric",
        100,
    )
    .expect("registration payload builds");
    let id: &str = id.as_ref();
    assert!(
        id.starts_with("gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat."),
        "unexpected plugin type id: {id}"
    );
    assert!(id.contains("audit"), "unexpected plugin type id: {id}");
    assert_eq!(payload["vendor"], "constructorfabric");
}

#[test]
fn both_plugin_specs_are_in_the_link_time_schema_inventory() {
    let schemas = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    let ids: Vec<String> = schemas
        .iter()
        .filter_map(|s| s["$id"].as_str().map(str::to_owned))
        .filter(|id| id.contains("plugin.v1~cf.core.mini_chat."))
        .collect();
    assert!(
        ids.iter().any(|id| id.contains("model_policy")),
        "model-policy plugin spec schema not registered: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("audit")),
        "audit plugin spec schema not registered: {ids:?}"
    );
}
