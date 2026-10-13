//! Static model policy service.

use async_trait::async_trait;
use mini_chat_sdk::credits::MAX_MULTIPLIER_MICRO;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, PolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use tracing::info;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// The only policy version this plugin serves.
const POLICY_VERSION: i64 = 1;

/// Serves the configured catalog, kill switches and limits as an immutable version 1.
#[derive(Debug)]
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    /// Validate the configuration and build the service.
    ///
    /// # Errors
    /// Returns a description of the first invalid catalog entry: a credit multiplier outside
    /// `1..=10_000_000_000` or a zero `estimation_budgets.bytes_per_token_conservative`.
    pub fn from_config(cfg: StaticModelPolicyConfig) -> Result<Self, String> {
        for entry in &cfg.model_catalog {
            for (name, value) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    entry.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    entry.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !(1..=MAX_MULTIPLIER_MICRO).contains(&value) {
                    return Err(format!(
                        "model `{}`: {name} multiplier must be in 1..={MAX_MULTIPLIER_MICRO}, got {value}",
                        entry.id
                    ));
                }
            }
            if entry.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model `{}`: estimation_budgets.bytes_per_token_conservative must be positive",
                    entry.id
                ));
            }
        }
        Ok(Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog,
                kill_switches: KillSwitches::from(cfg.kill_switches),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    fn check_version(policy_version: i64) -> Result<(), PolicyPluginError> {
        if policy_version == POLICY_VERSION {
            Ok(())
        } else {
            Err(PolicyPluginError::NotFound(format!(
                "policy version {policy_version}"
            )))
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, PolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(UserLimits {
            user_id,
            policy_version: POLICY_VERSION,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        info!(
            tenant_id = %event.tenant_id,
            request_id = %event.request_id,
            effective_model = %event.effective_model,
            billing_outcome = %event.billing_outcome,
            actual_credits_micro = event.actual_credits_micro,
            dedupe_key = %event.dedupe_key,
            "mini-chat usage event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, PolicyPluginError, TierLimits};
    use uuid::Uuid;

    use super::*;
    use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
    use crate::test_support::fixtures::catalog_entry;

    fn config_with(entry: mini_chat_sdk::ModelCatalogEntry) -> StaticModelPolicyConfig {
        StaticModelPolicyConfig {
            model_catalog: vec![entry],
            ..StaticModelPolicyConfig::default()
        }
    }

    #[tokio::test]
    async fn serves_catalog_as_version_1() {
        let user = Uuid::new_v4();
        let cfg = StaticModelPolicyConfig {
            model_catalog: vec![catalog_entry("a"), catalog_entry("b")],
            default_standard_limits: TierLimits {
                limit_daily_credits_micro: 11,
                limit_monthly_credits_micro: 22,
            },
            default_premium_limits: TierLimits {
                limit_daily_credits_micro: 33,
                limit_monthly_credits_micro: 44,
            },
            ..StaticModelPolicyConfig::default()
        };
        let svc = StaticModelPolicyService::from_config(cfg.clone()).unwrap();

        let version = svc.get_current_policy_version(user).await.unwrap();
        assert_eq!(version.policy_version, 1);

        let snapshot = svc.get_policy_snapshot(user, 1).await.unwrap();
        assert_eq!(snapshot.policy_version, 1);
        assert_eq!(snapshot.model_catalog, cfg.model_catalog);
        assert_eq!(
            snapshot.kill_switches,
            mini_chat_sdk::KillSwitches::from(cfg.kill_switches)
        );

        let limits = svc.get_user_limits(user, 1).await.unwrap();
        assert_eq!(limits.user_id, user);
        assert_eq!(limits.policy_version, 1);
        assert_eq!(limits.standard, cfg.default_standard_limits);
        assert_eq!(limits.premium, cfg.default_premium_limits);

        assert!(!svc.check_user_license(user).await.unwrap().active);

        assert!(matches!(
            svc.get_policy_snapshot(user, 2).await,
            Err(PolicyPluginError::NotFound(_))
        ));
        assert!(matches!(
            svc.get_user_limits(user, 2).await,
            Err(PolicyPluginError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn publish_usage_is_accepted() {
        let svc =
            StaticModelPolicyService::from_config(StaticModelPolicyConfig::default()).unwrap();
        let event = mini_chat_sdk::UsageEvent {
            tenant_id: Uuid::new_v4(),
            user_id: Some(Uuid::new_v4()),
            chat_id: Uuid::new_v4(),
            turn_id: Some(Uuid::new_v4()),
            request_id: Uuid::new_v4(),
            effective_model: "a".to_owned(),
            selected_model: "a".to_owned(),
            terminal_state: "completed".to_owned(),
            billing_outcome: "completed".to_owned(),
            usage: None,
            actual_credits_micro: 5,
            settlement_method: "actual".to_owned(),
            policy_version_applied: 1,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            requester_type: "user".to_owned(),
            dedupe_key: "k".to_owned(),
            system_task_type: None,
        };
        svc.publish_usage(event).await.unwrap();
    }

    #[test]
    fn rejects_zero_or_huge_multiplier() {
        let ok = catalog_entry("m");
        assert!(StaticModelPolicyService::from_config(config_with(ok.clone())).is_ok());

        for (input, output) in [
            (0, 1),
            (10_000_000_001, 1),
            (1, 0),
            (1, 10_000_000_001),
            (-5, 1),
        ] {
            let mut e = ok.clone();
            e.input_tokens_credit_multiplier_micro = input;
            e.output_tokens_credit_multiplier_micro = output;
            let err = StaticModelPolicyService::from_config(config_with(e)).unwrap_err();
            assert!(err.contains("`m`"), "{err}");
            assert!(err.contains("multiplier"), "{err}");
        }

        // Inclusive upper bound.
        let mut e = ok.clone();
        e.input_tokens_credit_multiplier_micro = 10_000_000_000;
        e.output_tokens_credit_multiplier_micro = 1;
        assert!(StaticModelPolicyService::from_config(config_with(e)).is_ok());

        let mut e = ok;
        e.estimation_budgets.bytes_per_token_conservative = 0;
        let err = StaticModelPolicyService::from_config(config_with(e)).unwrap_err();
        assert!(err.contains("bytes_per_token_conservative"), "{err}");
    }
}
