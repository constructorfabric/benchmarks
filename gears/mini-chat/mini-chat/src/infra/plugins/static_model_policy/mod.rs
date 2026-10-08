//! Bundled static model policy plugin gear
//! (`static-mini-chat-model-policy-plugin`).
//!
//! Serves a fixed policy (version 1) from its config: model catalog, kill
//! switches and the same per-user limits for every user. `publish_usage` only
//! logs the event.

pub mod config;

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use self::config::StaticModelPolicyConfig;

/// GTS instance segment of the bundled plugin.
pub const INSTANCE_SEGMENT: &str = "cf.builtin.static_mini_chat_model_policy.plugin.v1";

/// The fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

const MIN_MULTIPLIER: u64 = 1;
const MAX_MULTIPLIER: u64 = 10_000_000_000;

/// Static policy plugin client.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    /// Build from config, validating credit multipliers (`1..=10_000_000_000`)
    /// and `estimation_budgets.bytes_per_token_conservative > 0`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first invalid catalog entry.
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> anyhow::Result<Self> {
        for e in &cfg.model_catalog {
            for (field, value) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    e.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    e.output_tokens_credit_multiplier_micro,
                ),
            ] {
                anyhow::ensure!(
                    (MIN_MULTIPLIER..=MAX_MULTIPLIER).contains(&value),
                    "model_catalog entry {:?}: {field} must be in {MIN_MULTIPLIER}..={MAX_MULTIPLIER}, got {value}",
                    e.id
                );
            }
            anyhow::ensure!(
                e.estimation_budgets.bytes_per_token_conservative > 0,
                "model_catalog entry {:?}: estimation_budgets.bytes_per_token_conservative must be > 0",
                e.id
            );
        }
        Ok(Self {
            snapshot: PolicySnapshot {
                policy_version: STATIC_POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.into(),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
            generated_at: OffsetDateTime::now_utc(),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: STATIC_POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    /// The static policy has a single version; any requested version is
    /// answered with it (so turns settled after a plugin swap still resolve).
    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version: STATIC_POLICY_VERSION,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        info!(
            tenant_id = %event.tenant_id,
            chat_id = %event.chat_id,
            request_id = %event.request_id,
            dedupe_key = %event.dedupe_key,
            actual_credits_micro = event.actual_credits_micro,
            billing_outcome = ?event.billing_outcome,
            "static model policy plugin: usage event received"
        );
        Ok(())
    }
}

/// Static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticModelPolicyPlugin;

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        let service = StaticModelPolicyService::from_config(&cfg)?;

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                &cfg.vendor,
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = Arc::new(service);
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(
            instance_id = %instance_id,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "static mini-chat model policy plugin registered"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
